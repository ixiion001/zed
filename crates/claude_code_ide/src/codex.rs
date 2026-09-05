//! Native Codex `/ide`, intentionally independent of Claude's MCP adapter.
mod protocol;
mod router;
mod routing;
mod service;
#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

use crate::editor_context::{bounded_text, local_abs_path, utf16_range};
use editor::Editor;
use futures::StreamExt;
use gpui::{App, EntityId, Global, Subscription, Task, WeakEntity, actions};
use serde_json::{Value, json};
use std::{
    cell::RefCell,
    path::{Path, PathBuf},
    rc::Rc,
    time::SystemTime,
};
use workspace::{Toast, Workspace, notifications::NotificationId};

/// Availability describes the provider's registration, never connected terminals.
/// CLI context is opt-in and requests are short lived; `/ide status` remains the
/// authority for whether context is enabled in a particular CLI session.
#[derive(Default, Clone)]
pub struct CodexIdeStatus {
    pub available: bool,
    pub last_successful_request: Option<SystemTime>,
    pub last_error: Option<String>,
}
impl Global for CodexIdeStatus {}
actions!(codex, [ShowStatus]);
struct WindowContext {
    id: EntityId,
    workspace: WeakEntity<Workspace>,
    last_editor: Option<WeakEntity<Editor>>,
    _subscription: Subscription,
}
struct Integration {
    owned_endpoint: service::OwnedEndpoint,
    _transport: Task<Result<(), gpui_tokio::JoinError>>,
    _foreground: Task<()>,
}
impl Global for Integration {}

pub fn init(cx: &mut App) {
    if cx.has_global::<Integration>() {
        return;
    }
    cx.set_global(CodexIdeStatus::default());
    let registry: Rc<RefCell<Vec<WindowContext>>> = Rc::default();
    cx.observe_new({
        let registry = registry.clone();
        move |workspace: &mut Workspace, _, cx: &mut gpui::Context<Workspace>| {
            if !workspace.project().read(cx).is_local() { return; }
            let id = cx.entity_id();
            let subscription = cx.subscribe(&cx.entity(), {
                let registry = registry.clone();
                move |workspace, _, event, cx| {
                    if matches!(event, workspace::Event::ActiveItemChanged) {
                        if let Some(editor) = workspace.active_item_as::<Editor>(cx) {
                            if editor_path(&editor, cx).is_some() {
                                if let Some(entry) = registry.borrow_mut().iter_mut().find(|entry| entry.id == id) {
                                    entry.last_editor = Some(editor.downgrade());
                                }
                            }
                        }
                    }
                }
            });
            registry.borrow_mut().push(WindowContext { id, workspace: cx.entity().downgrade(), last_editor: None, _subscription: subscription });
            workspace.register_action(|workspace, _: &ShowStatus, _, cx| {
                let state = cx.global::<CodexIdeStatus>();
                let availability = if state.available { "available" } else { "unavailable" };
                let last = state.last_successful_request.and_then(|time| time.elapsed().ok())
                    .map(|elapsed| format!("Last successful request {}s ago.", elapsed.as_secs()))
                    .unwrap_or_else(|| "No successful context request yet.".into());
                let error = state.last_error.as_deref().map(|e| format!(" {e}")).unwrap_or_default();
                let message = format!("Codex IDE provider {availability}. {last}{error} Use /ide status in Codex to check whether context is enabled.");
                workspace.show_toast(Toast::new(NotificationId::named("codex-ide-status".into()), message), cx);
            });
            cx.on_release({ let registry = registry.clone(); move |_, _| { registry.borrow_mut().retain(|entry| entry.id != id); } }).detach();
        }
    }).detach();
    let (events, mut incoming) = futures::channel::mpsc::unbounded();
    let home = std::env::var_os("CODEX_HOME")
        .filter(|home| !home.is_empty())
        .map(PathBuf::from)
        .unwrap_or_else(|| paths::home_dir().join(".codex"));
    let owned_endpoint = service::OwnedEndpoint::default();
    let transport =
        gpui_tokio::Tokio::spawn(cx, service::run(home, events, owned_endpoint.clone()));
    let foreground = cx.spawn(async move |cx| {
        while let Some(event) = incoming.next().await {
            match event {
                service::Event::Query {
                    directory,
                    discovery,
                    reply,
                } => {
                    if reply.is_closed() {
                        continue;
                    }
                    let (ids, roots): (Vec<_>, Vec<_>) = cx
                        .update(|cx| workspace_roots(&registry.borrow(), cx))
                        .into_iter()
                        .unzip();
                    // Matching canonicalizes paths, which can block on a slow
                    // mount; that must not stall the editor.
                    let selected = cx
                        .background_executor()
                        .spawn(async move {
                            routing::select(&directory, &roots).map(|(index, _)| index)
                        })
                        .await;
                    let result = match selected {
                        Ok(_) if discovery => Ok(Value::Null),
                        Ok(index) => match ids.get(index) {
                            Some(&id) => cx.update(|cx| context(&registry.borrow(), id, cx)),
                            None => Err("workspace-closed"),
                        },
                        Err(error) => Err(error),
                    };
                    let _ = reply.send(result.map_err(str::to_owned));
                }
                service::Event::Status {
                    available,
                    error,
                    successful_request,
                } => {
                    cx.update(|cx| {
                        let status = cx.global_mut::<CodexIdeStatus>();
                        if status.available != available {
                            log::info!("Codex IDE provider available: {available}");
                        }
                        if status.last_error != error {
                            if let Some(error) = &error {
                                log::warn!("Codex IDE: {error}");
                            }
                        }
                        status.available = available;
                        status.last_error = error;
                        if let Some(time) = successful_request {
                            status.last_successful_request = Some(time);
                        }
                    });
                }
            }
        }
        // The sender only closes when the transport task has died, since a
        // normal quit drops this task first. Say so instead of staying green.
        log::warn!("Codex IDE transport stopped unexpectedly");
        cx.update(|cx| {
            let status = cx.global_mut::<CodexIdeStatus>();
            status.available = false;
            status.last_error = Some("Codex IPC transport stopped unexpectedly".into());
        });
    });
    cx.set_global(Integration {
        owned_endpoint,
        _transport: transport,
        _foreground: foreground,
    });
    // Aborting the transport is asynchronous and on macOS the runtime is never
    // dropped before exit, so unlink an owned socket here, synchronously.
    cx.on_app_quit(|cx| {
        let integration = cx.remove_global::<Integration>();
        if let Ok(owned) = integration.owned_endpoint.lock() {
            if let Some(identity) = owned.as_ref() {
                identity.unlink_if_current();
            }
        }
        async {}
    })
    .detach();
}
fn editor_path(editor: &gpui::Entity<Editor>, cx: &App) -> Option<PathBuf> {
    let editor = editor.read(cx);
    let buffer = editor.buffer().read(cx).as_singleton()?;
    local_abs_path(buffer.read(cx), cx).map(PathBuf::from)
}
fn descriptor(path: &Path) -> Value {
    // Absolute native paths remain correct when the CLI runs in a nested cwd.
    json!({"label":path.file_name().unwrap_or_default().to_string_lossy(), "path":path, "fsPath":path})
}
fn workspace_roots(registry: &[WindowContext], cx: &App) -> Vec<(EntityId, Vec<PathBuf>)> {
    registry
        .iter()
        .map(|entry| {
            let roots = entry
                .workspace
                .upgrade()
                .map(|workspace| {
                    let project = workspace.read(cx).project().read(cx);
                    if !project.is_local() {
                        return vec![];
                    }
                    project
                        .visible_worktrees(cx)
                        .map(|tree| tree.read(cx).abs_path().to_path_buf())
                        .collect()
                })
                .unwrap_or_default();
            (entry.id, roots)
        })
        .collect()
}
fn context(registry: &[WindowContext], id: EntityId, cx: &mut App) -> Result<Value, &'static str> {
    // The window may have closed while the match ran off the foreground.
    let entry = registry
        .iter()
        .find(|entry| entry.id == id)
        .ok_or("workspace-closed")?;
    let workspace = entry.workspace.upgrade().ok_or("workspace-closed")?;
    let workspace = workspace.read(cx);
    let editors = workspace.items_of_type::<Editor>(cx).collect::<Vec<_>>();
    let active = workspace
        .active_item_as::<Editor>(cx)
        .filter(|editor| editor_path(editor, cx).is_some())
        .or_else(|| {
            entry
                .last_editor
                .as_ref()?
                .upgrade()
                .filter(|editor| editors.contains(editor))
        });
    let tabs = editors
        .iter()
        .filter_map(|editor| editor_path(editor, cx))
        .take(128)
        .map(|path| descriptor(&path))
        .collect::<Vec<_>>();
    let active = active.and_then(|editor| {
        let path = editor_path(&editor, cx)?;
        Some(editor.update(cx, |editor, cx| {
            let display = editor.display_snapshot(cx);
            let range = editor.selections.newest::<text::Point>(&display).range();
            let ranges = editor.selections.all::<text::Point>(&display);
            let snapshot = editor.buffer().read(cx).snapshot(cx);
            let make_range = |range| {
                let (start, end) = utf16_range(&snapshot, range);
                json!({"start":{"line":start.row,"character":start.column},"end":{"line":end.row,"character":end.column}})
            };
            let mut file = descriptor(&path);
            file["selection"] = make_range(range.clone());
            file["selections"] = ranges.into_iter().take(1024).map(|selection| make_range(selection.range())).collect();
            // Bound the live selection without serializing the whole unsaved file.
            file["activeSelectionContent"] = json!(bounded_text(snapshot.text_for_range(range), 200_000).0);
            file
        }))
    });
    Ok(json!({"activeFile":active,"openTabs":tabs}))
}
