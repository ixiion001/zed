//! IDE-side implementation of the Claude Code editor integration protocol.
//!
//! This lets the Claude Code CLI connect to Zed the same way it connects to the
//! official VS Code and JetBrains extensions: Zed runs a localhost WebSocket
//! server speaking a WebSocket variant of MCP, advertises it through a lock file
//! under `~/.claude/ide/`, and points the CLI at it via the `CLAUDE_CODE_SSE_PORT`
//! and `ENABLE_IDE_INTEGRATION` environment variables in its integrated terminal.
//!
//! [`init`] wires one [`ClaudeCodeIdeServer`] per local [`Workspace`]: it binds
//! a loopback port, writes the lock file, serves connections until the window
//! closes, and pushes `selection_changed` to every connected CLI as the user
//! moves around, which is the only way the CLI learns what is selected.
//!
//! The same startup hook registers a private native Codex IPC provider. Its
//! protocol, router and workspace registry live in `codex`; see `CODEX.md` for
//! the pinned compatibility evidence and remaining platform acceptance gates.

mod codex;
mod editor_context;
mod lockfile;
mod open_diff;
mod server;
mod tools;

pub use codex::CodexIdeStatus;

use std::{cell::RefCell, rc::Rc, time::Duration};

use anyhow::{Context as _, Result};
use collections::HashMap;
use editor::{Editor, EditorEvent};
use futures::channel::mpsc;
use gpui::{
    AnyWindowHandle, App, AppContext as _, AsyncApp, Context, Entity, EntityId, Subscription, Task,
    WeakEntity, actions,
};
use project::Project;
use serde_json::{Value, json};
use terminal_view::terminal_panel::TerminalPanel;
use util::ResultExt as _;
use workspace::{Toast, Workspace, notifications::NotificationId};

use lockfile::generate_auth_token;
use server::{bind, serve_connection};
use tools::{WorkspaceDispatcher, selection_payload, selection_range};

actions!(
    claude_code_ide,
    [
        /// Inserts the active editor's file and selected lines into the Claude
        /// Code prompt as an `@` mention, like Cmd+Option+K in VS Code.
        MentionSelection
    ]
);

/// Registers a Claude Code IDE server for every local workspace window.
///
/// Call once during app startup. Each created [`Workspace`] gets its own server
/// entity, kept alive in `servers` for the window's lifetime and dropped (which
/// removes its lock file) when the workspace is released.
pub fn init(cx: &mut App) {
    codex::init(cx);
    let servers: Rc<RefCell<HashMap<EntityId, Entity<ClaudeCodeIdeServer>>>> = Rc::default();
    cx.observe_new({
        let servers = servers.clone();
        move |workspace: &mut Workspace, window, cx: &mut Context<Workspace>| {
            // The server binds this machine's loopback and reads local buffers.
            // An SSH, WSL or collab window would advertise paths that mean
            // nothing here and answer with what it cannot see.
            let project = workspace.project().clone();
            if !project.read(cx).is_local() {
                return;
            }
            let workspace_id = cx.entity_id();
            let workspace_handle = cx.entity();
            let window_handle = window.map(|window| window.window_handle());
            let server = cx.new(|cx| {
                ClaudeCodeIdeServer::new(&workspace_handle, &project, window_handle, cx)
            });
            // The mention comes from the workspace's active item rather than
            // from focus, so the shortcut works while the user is typing in
            // Claude's terminal.
            workspace.register_action({
                let server = server.downgrade();
                move |workspace, _: &MentionSelection, window, cx| {
                    let outcome = match workspace.active_item_as::<Editor>(cx) {
                        Some(editor) => server
                            .update(cx, |server, cx| server.push_at_mention(&editor, cx))
                            .unwrap_or(Err("this window has no Claude Code server")),
                        None => Err("open a file in the centre pane to mention its selection"),
                    };
                    match outcome {
                        // The mention now sits in the CLI's prompt, so Enter should
                        // submit it there rather than overwrite the selection it
                        // came from.
                        Ok(()) => {
                            workspace.focus_panel::<TerminalPanel>(window, cx);
                        }
                        // A shortcut that does nothing leaves the user guessing why.
                        Err(reason) => {
                            let id = NotificationId::named("claude-code-ide-mention".into());
                            workspace
                                .show_toast(Toast::new(id, format!("Claude Code: {reason}")), cx);
                        }
                    }
                }
            });
            servers.borrow_mut().insert(workspace_id, server);

            cx.on_release({
                let servers = servers.clone();
                move |_workspace, _cx| {
                    servers.borrow_mut().remove(&workspace_id);
                }
            })
            .detach();
        }
    })
    .detach();

    // Lock files are removed when a window closes (see `Drop`), but a hard quit
    // skips destructors, so clean them up explicitly on app exit too.
    cx.on_app_quit({
        let servers = servers.clone();
        move |cx| {
            for server in servers.borrow().values() {
                server.update(cx, |server, _| server.remove_lockfile());
            }
            async move {}
        }
    })
    .detach();
}

/// One running WebSocket server, bound to a single workspace window.
struct ClaudeCodeIdeServer {
    /// Set once the listener is bound; read by `Drop` to remove the lock file.
    port: Option<u16>,
    auth_token: String,
    /// One sender per live connection. Whatever is sent here goes out as a text
    /// frame, and dropping the sender -- with this entity -- ends that
    /// connection's loop, so no connection outlives its window.
    connections: Vec<mpsc::UnboundedSender<String>>,
    /// Follows the selection of whichever editor is active.
    editor_subscription: Option<Subscription>,
    _subscriptions: Vec<Subscription>,
    /// The bind + accept loop. Dropping it (when the workspace closes) cancels
    /// the loop, stopping the server.
    _server_task: Task<()>,
}

impl ClaudeCodeIdeServer {
    fn new(
        workspace: &Entity<Workspace>,
        project: &Entity<Project>,
        window: Option<AnyWindowHandle>,
        cx: &mut Context<Self>,
    ) -> Self {
        // The workspace is still being constructed here (`observe_new`), so it
        // can be subscribed to but not read; it has no items yet anyway.
        let subscriptions = vec![
            cx.subscribe(workspace, |this, workspace, event, cx| {
                if let workspace::Event::ActiveItemChanged = event {
                    this.follow_active_editor(&workspace, cx);
                }
            }),
            // The lock file names the folders the CLI matches its working
            // directory against, so it has to follow Add/Remove Folder.
            cx.subscribe(project, |this, project, event, cx| {
                if let project::Event::WorktreeAdded(_) | project::Event::WorktreeRemoved(_) =
                    event
                {
                    this.write_lockfile(&project, cx).log_err();
                }
            }),
        ];
        let server_task = cx.spawn({
            let workspace = workspace.downgrade();
            let project = project.downgrade();
            async move |this, cx| {
                if let Err(error) = Self::run(this, workspace, project, window, cx).await {
                    log::error!("Claude Code IDE server stopped: {error:#}");
                }
            }
        });
        Self {
            port: None,
            auth_token: generate_auth_token(),
            connections: Vec::new(),
            editor_subscription: None,
            _subscriptions: subscriptions,
            _server_task: server_task,
        }
    }

    async fn run(
        this: WeakEntity<Self>,
        workspace: WeakEntity<Workspace>,
        project: WeakEntity<Project>,
        window: Option<AnyWindowHandle>,
        cx: &mut AsyncApp,
    ) -> Result<()> {
        let (listener, port) = bind().await?;

        let auth_token = this.update(cx, |this, cx| {
            this.port = Some(port);
            let project = project.upgrade().context("project released")?;
            this.write_lockfile(&project, cx)?;
            // Publish the port to the project so newly opened terminals
            // advertise it to the Claude CLI via `CLAUDE_CODE_SSE_PORT`.
            project.update(cx, |project, _| project.set_claude_code_ide_port(Some(port)));
            anyhow::Ok(this.auth_token.clone())
        })??;

        log::info!("Claude Code IDE server listening on 127.0.0.1:{port}");

        // Each accepted connection is served on the foreground executor so its
        // tool handlers can touch workspace entities; the async I/O still yields,
        // so it never blocks the UI.
        // A failed accept used to end the loop and return Ok(()), which left the
        // lock file advertising a port nothing was listening on and logged
        // nothing at all -- the CLI would connect, fail, and give no clue why.
        // Most accept errors are transient (a peer that went away between the
        // SYN and our accept, a momentary descriptor shortage), so carry on;
        // give up only if they stop being occasional, which means the listener
        // itself is gone.
        let mut consecutive_failures = 0u32;
        loop {
            let (stream, _addr) = match listener.accept().await {
                Ok(accepted) => {
                    consecutive_failures = 0;
                    accepted
                }
                Err(error) => {
                    consecutive_failures += 1;
                    log::warn!(
                        "Claude Code IDE: accept failed ({consecutive_failures}): {error}"
                    );
                    if consecutive_failures >= 16 {
                        log::error!(
                            "Claude Code IDE: giving up on the listener after \
                             {consecutive_failures} consecutive accept failures"
                        );
                        break;
                    }
                    // A descriptor shortage is reported without yielding, so
                    // without a pause all sixteen tries would pass in one poll.
                    cx.background_executor()
                        .timer(Duration::from_millis(250) * consecutive_failures)
                        .await;
                    continue;
                }
            };
            let (outbound_tx, outbound_rx) = mpsc::unbounded();
            if this.update(cx, |this, _| this.connections.push(outbound_tx)).is_err() {
                // The window is gone, and with it everything a connection could
                // be served from.
                return Ok(());
            }
            let dispatcher = WorkspaceDispatcher::new(workspace.clone(), window, cx.clone());
            let auth_token = auth_token.clone();
            cx.spawn(async move |_cx| {
                serve_connection(stream, auth_token, dispatcher, outbound_rx).await.log_err();
            })
            .detach();
        }

        // Reaching here means the listener is beyond saving while the window is
        // still open, so the port has to be withdrawn: terminals opened from now
        // on would otherwise export CLAUDE_CODE_SSE_PORT for a port nothing
        // answers on, and the CLI would try to connect and fail instead of
        // simply running without the integration.
        //
        // The lock file goes first, so a failure to reach the project cannot
        // leave one behind advertising a dead port. Clearing the port after it
        // also makes `remove_lockfile` in Drop a no-op rather than a double
        // removal.
        lockfile::remove(port).log_err();
        this.update(cx, |this, cx| {
            this.port = None;
            if let Some(project) = project.upgrade() {
                project.update(cx, |project, _| project.set_claude_code_ide_port(None));
            }
        })
        .log_err();

        Ok(())
    }

    /// Writes the lock file for the bound port and the project's visible
    /// worktrees; a no-op until the listener is bound.
    fn write_lockfile(&self, project: &Entity<Project>, cx: &App) -> Result<()> {
        let Some(port) = self.port else {
            return Ok(());
        };
        let folders = project
            .read(cx)
            .visible_worktrees(cx)
            .map(|worktree| worktree.read(cx).abs_path().to_path_buf())
            .collect::<Vec<_>>();
        lockfile::create(port, &self.auth_token, &folders)?;
        Ok(())
    }

    /// Removes this server's lock file, if one has been written. Idempotent.
    fn remove_lockfile(&self) {
        if let Some(port) = self.port {
            lockfile::remove(port).log_err();
        }
    }

    fn follow_active_editor(&mut self, workspace: &Entity<Workspace>, cx: &mut Context<Self>) {
        // Anything else becoming active (a diff tab, an image, a panel) keeps
        // the previous subscription, so selections keep flowing from the editor
        // the user was last in.
        let Some(editor) = workspace.read(cx).active_item_as::<Editor>(cx) else {
            return;
        };
        self.editor_subscription = Some(cx.subscribe(&editor, |this, editor, event, cx| {
            if let EditorEvent::SelectionsChanged { local: true } = event {
                this.push_selection(&editor, cx);
            }
        }));
        self.push_selection(&editor, cx);
    }

    /// Tells every connected CLI what is selected. The CLI never asks; this
    /// push is what puts "N lines selected" in its footer and the selection
    /// into the model's context.
    fn push_selection(&mut self, editor: &Entity<Editor>, cx: &mut App) {
        self.broadcast("selection_changed", selection_payload(editor, cx));
    }

    /// Asks every connected CLI to insert `@file#Lstart-end` into its prompt,
    /// or says why it cannot. Rows go out 0-based, as the CLI adds one itself.
    /// A selection that ends at column 0 of the next line does not include
    /// that line, which is how the CLI's own "N lines selected" counts it.
    fn push_at_mention(
        &mut self,
        editor: &Entity<Editor>,
        cx: &mut App,
    ) -> Result<(), &'static str> {
        if self.connections.is_empty() {
            return Err("no CLI session is connected to this window");
        }
        let (path, range) = selection_range(editor, cx);
        if path.is_empty() {
            return Err("save the file before mentioning it");
        }
        let mut line_end = range.end.row;
        if range.end.column == 0 && line_end > range.start.row {
            line_end -= 1;
        }
        self.broadcast(
            "at_mentioned",
            json!({ "filePath": path, "lineStart": range.start.row, "lineEnd": line_end }),
        );
        Ok(())
    }

    fn broadcast(&mut self, method: &str, params: Value) {
        let notification =
            json!({ "jsonrpc": "2.0", "method": method, "params": params }).to_string();
        self.connections
            .retain(|connection| connection.unbounded_send(notification.clone()).is_ok());
    }
}

impl Drop for ClaudeCodeIdeServer {
    fn drop(&mut self) {
        self.remove_lockfile();
    }
}
