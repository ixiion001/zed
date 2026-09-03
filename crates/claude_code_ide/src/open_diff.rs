//! The blocking `openDiff` tool.
//!
//! Shows Claude's proposed change as a side-by-side diff tab in Zed plus a
//! Keep/Reject notification, then blocks until the user decides. On Keep we
//! return `FILE_SAVED` together with the final buffer contents and let the
//! Claude CLI perform the actual write (matching the official IDE protocol);
//! on Reject we return `DIFF_REJECTED` and discard the change. The IDE
//! deliberately does *not* write the file itself, which would race the CLI's
//! own save.

use crate::server::{ProtocolError, error_codes};
use buffer_diff::{BufferDiff, DiffBaseKind};
use collections::HashMap;
use editor::{DiffViewStyle, MultiBuffer, SelectionEffects, SplittableEditor, scroll::Autoscroll};
use futures::channel::oneshot;
use gpui::{AnyWindowHandle, AppContext as _, AsyncApp, DismissEvent, TaskExt as _, WeakEntity};
use language::Buffer;
use serde_json::{Value, json};
use ui::{Color, IconName};
use std::{
    cell::RefCell,
    rc::{Rc, Weak},
};
use workspace::{
    SaveIntent, SplitDirection, Workspace,
    notifications::{NotificationId, simple_message_notification::MessageNotification},
};

/// Distinguishes our notification from others in the notification registry.
struct ClaudeDiffNotification;

/// The diffs one connection is waiting on, keyed by the CLI's `tab_name`.
///
/// The map owns each request's decision sender, so an entry *is* the open
/// request: removing it without an answer rejects it (the receiver reads a
/// dropped sender as `false`), and dropping the whole map -- the connection is
/// gone -- rejects everything still pending, which is what lets each diff's
/// task clean up after a CLI that died mid-decision. Callbacks that can outlive
/// the connection hold only a [`Weak`] to the map, so they never keep those
/// requests alive.
pub type PendingDiffs = Rc<Pending>;
type Pending = RefCell<HashMap<String, oneshot::Sender<bool>>>;

/// Settles the request for `tab_name`, if it is still pending.
fn resolve(pending: &Weak<Pending>, tab_name: &str, accepted: bool) {
    let Some(pending) = pending.upgrade() else {
        return;
    };
    let sender = pending.borrow_mut().remove(tab_name);
    if let Some(sender) = sender {
        sender.send(accepted).ok();
    }
}

pub async fn open_diff(
    workspace: WeakEntity<Workspace>,
    window: Option<AnyWindowHandle>,
    pending: &PendingDiffs,
    arguments: Value,
    cx: &mut AsyncApp,
) -> Result<Value, ProtocolError> {
    let string_arg = |key: &str| arguments.get(key).and_then(Value::as_str).map(str::to_owned);

    let old_file_path = string_arg("old_file_path")
        .ok_or_else(|| ProtocolError::new(error_codes::INVALID_REQUEST, "missing old_file_path"))?;
    let new_file_contents = string_arg("new_file_contents").ok_or_else(|| {
        ProtocolError::new(error_codes::INVALID_REQUEST, "missing new_file_contents")
    })?;
    let tab_name = string_arg("tab_name").unwrap_or_else(|| "Proposed changes".to_owned());

    let window =
        window.ok_or_else(|| ProtocolError::internal("no window available to show a diff"))?;

    // The current on-disk contents are the diff base (the "old" side).
    let old_contents = {
        let path = old_file_path.clone();
        smol::unblock(move || match std::fs::read_to_string(&path) {
            Ok(contents) => Ok(contents),
            // A file that does not exist yet is the create case: an empty base
            // is right, and the whole proposal shows as added.
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
            Err(error) => Err(error),
        })
        .await
        // Anything else -- a permission error, or contents that are not UTF-8 --
        // used to fall back to an empty base, which renders as "the file is
        // empty and all of this is new". Keep would then return the proposal as
        // the whole file and the CLI would write it, silently discarding
        // contents the user was never shown. Refusing is the only safe answer.
        .map_err(|error| {
            ProtocolError::internal(format!("cannot read {old_file_path} to diff against: {error}"))
        })?
    };

    let message = format!("Claude proposes changes to {old_file_path}");

    // Build the proposed buffer and start computing the diff against the old
    // contents (the base). `set_base_text` is asynchronous, so we await it
    // before showing the editor to avoid flashing a "whole file added" diff.
    let (buffer, diff, base_ready) = window
        .update(cx, |_root, _window, cx| {
            let buffer = cx.new(|cx| Buffer::local(new_file_contents.clone(), cx));
            let language = buffer.read(cx).language().cloned();
            let language_registry = buffer.read(cx).language_registry();
            let snapshot = buffer.read(cx).text_snapshot();
            // `Custom` because the base is caller-provided text (the file as it
            // is on disk) rather than anything from git.
            let diff = cx.new(|cx| {
                BufferDiff::new(
                    &snapshot,
                    language,
                    language_registry,
                    DiffBaseKind::Custom,
                    cx,
                )
            });
            let base_ready = diff.update(cx, |diff, cx| {
                diff.set_base_text(Some(old_contents.into()), snapshot, cx)
            });
            (buffer, diff, base_ready)
        })
        .map_err(|error| ProtocolError::internal(error.to_string()))?;

    base_ready.await;

    let notification_id = NotificationId::composite::<ClaudeDiffNotification>(tab_name.clone());
    let weak_pending = Rc::downgrade(pending);
    let editor_id = window
        .update(cx, |_root, window, cx| {
            let multibuffer = cx.new(|cx| {
                let mut multibuffer = MultiBuffer::singleton(buffer.clone(), cx);
                multibuffer.add_diff(diff, cx);
                // A buffer without a file is otherwise titled by its first line.
                multibuffer.set_title(tab_name.clone(), cx);
                multibuffer
            });

            let workspace_entity = workspace.upgrade()?;
            let project = workspace_entity.read(cx).project().clone();

            // `DiffViewStyle::Split` renders side-by-side (old on the left, new on
            // the right) like the JetBrains diff viewer; `SplittableEditor` is
            // itself a workspace item, so it can be opened directly as a tab.
            let diff_editor = cx.new(|cx| {
                SplittableEditor::new(
                    DiffViewStyle::Split,
                    multibuffer,
                    project,
                    workspace_entity.clone(),
                    window,
                    cx,
                )
            });
            let editor_id = diff_editor.entity_id();

            // Closing the tab by hand is a rejection too. The observer runs inside
            // the app borrow, so it touches nothing but the map.
            cx.observe_release(&diff_editor, {
                let pending = weak_pending.clone();
                let tab_name = tab_name.clone();
                move |_, _| resolve(&pending, &tab_name, false)
            })
            .detach();

            workspace_entity.update(cx, |workspace, cx| {
                // Claude's terminal normally lives in the bottom dock, whose pane
                // is never `active_pane` (that tracks centre panes only), so only
                // `focused_pane` sees where the CLI really is. Show the diff in
                // the first centre pane that is not it, without taking focus: the
                // user is mid-conversation in the terminal and decides through
                // the notification. Split only if the focused pane is the sole one.
                let focused_pane = workspace.focused_pane(window, cx);
                let other_pane = workspace
                    .panes()
                    .iter()
                    .find(|pane| **pane != focused_pane)
                    .cloned();
                if let Some(target_pane) = other_pane {
                    workspace.add_item(
                        target_pane,
                        Box::new(diff_editor.clone()),
                        None,
                        false,
                        false,
                        window,
                        cx,
                    );
                } else {
                    workspace.split_item(
                        SplitDirection::Left,
                        Box::new(diff_editor.clone()),
                        window,
                        cx,
                    );
                }

                workspace.show_notification(notification_id.clone(), cx, {
                    let pending = weak_pending.clone();
                    let tab_name = tab_name.clone();
                    move |cx| {
                        let notification = cx.new(|cx| {
                            MessageNotification::new(message, cx)
                                .primary_message("Keep")
                                .primary_icon(IconName::Check)
                                .primary_icon_color(Color::Success)
                                .primary_on_click({
                                    let pending = pending.clone();
                                    let tab_name = tab_name.clone();
                                    move |_window, cx| {
                                        resolve(&pending, &tab_name, true);
                                        cx.emit(DismissEvent);
                                    }
                                })
                                .secondary_message("Reject")
                                .secondary_icon(IconName::Close)
                                .secondary_icon_color(Color::Error)
                                .secondary_on_click(|_window, cx| cx.emit(DismissEvent))
                        });
                        // Reject, the close button and "don't show again" all end
                        // in a dismissal, and a dismissed request nobody kept is
                        // rejected. Keep's handler runs before this event is
                        // delivered, so its answer wins.
                        cx.subscribe(&notification, move |_, _, _: &DismissEvent, _| {
                            resolve(&pending, &tab_name, false)
                        })
                        .detach();
                        notification
                    }
                });
            });

            // Center the view on the first change so the user lands on the diff
            // rather than at the top of an otherwise-unchanged file.
            diff_editor.update(cx, |diff_editor, cx| {
                let editor = diff_editor.rhs_editor().clone();
                editor.update(cx, |editor, cx| {
                    let snapshot = editor.buffer().read(cx).snapshot(cx);
                    if let Some(first_hunk) = snapshot.diff_hunks().next() {
                        let start = first_hunk.multi_buffer_range.start;
                        editor.change_selections(
                            SelectionEffects::scroll(Autoscroll::center()),
                            window,
                            cx,
                            |selections| selections.select_anchor_ranges([start..start]),
                        );
                    }
                });
            });
            Some(editor_id)
        })
        .map_err(|error| ProtocolError::internal(error.to_string()))?
        .ok_or_else(|| ProtocolError::internal("workspace closed before the diff could open"))?;

    // Registered only now that the tab and the notification exist, so an error
    // above leaves nothing to reject. A repeated `tab_name` replaces the earlier
    // sender, which rejects that request rather than leave two tabs racing.
    let (decision_tx, decision_rx) = oneshot::channel::<bool>();
    pending.borrow_mut().insert(tab_name, decision_tx);

    // The wait and the cleanup run in their own task: a connection dying
    // mid-diff cancels this future, but the map that died with it dropped our
    // sender, so the task wakes with `Err`, reads it as rejected, and still
    // removes the tab and the notification.
    let (result_tx, result_rx) = oneshot::channel();
    cx.spawn({
        let workspace = workspace.clone();
        async move |cx| {
            let accepted = decision_rx.await.unwrap_or(false);
            // Read the final buffer contents (the user may have edited the
            // proposed side) and close the diff tab now that the decision is made.
            let final_contents = window.update(cx, |_root, window, cx| {
                let final_contents = buffer.read(cx).text();
                if let Some(workspace) = workspace.upgrade() {
                    workspace.update(cx, |workspace, cx| {
                        workspace.dismiss_notification(&notification_id, cx);
                        for pane in workspace.panes().to_vec() {
                            pane.update(cx, |pane, cx| {
                                pane.close_item_by_id(editor_id, SaveIntent::Skip, window, cx)
                            })
                            .detach_and_log_err(cx);
                        }
                    });
                }
                final_contents
            });
            result_tx.send((accepted, final_contents)).ok();
        }
    })
    .detach();

    let (accepted, final_contents) = result_rx
        .await
        .map_err(|_| ProtocolError::internal("the diff view went away before a decision"))?;
    let final_contents =
        final_contents.map_err(|error| ProtocolError::internal(error.to_string()))?;

    // The official IDE protocol has the IDE return the accepted contents and the
    // CLI perform the write, so we must not write the file here ourselves.
    if accepted {
        Ok(json!({ "content": [
            { "type": "text", "text": "FILE_SAVED" },
            { "type": "text", "text": final_contents },
        ] }))
    } else {
        Ok(json!({ "content": [{ "type": "text", "text": "DIFF_REJECTED" }] }))
    }
}
