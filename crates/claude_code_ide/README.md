# claude_code_ide

Native [Claude Code](https://docs.claude.com/en/docs/claude-code) IDE integration
for Zed.

This crate lets the `claude` CLI connect to Zed the same way it connects to
VS Code, JetBrains and Neovim — over Claude Code's native WebSocket + MCP
"IDE integration" protocol. It is **not** ACP: Zed already exposes Claude Code
as an ACP agent in the agent panel; this is the complementary direction, where
`claude` running in Zed's integrated terminal drives the editor.

## What you get

- **Auto-connect** in Zed's integrated terminal — no `/ide` needed.
- **Your selection follows you.** Zed pushes `selection_changed` as you move
  around, which is the only way the CLI learns the active file and selection:
  its footer shows "N lines selected" / "In `<file>`", and the selection goes
  to the model as context.
- **`@`-mention shortcut.** `ctrl-alt-k` (`cmd-alt-k` on macOS) inserts the active editor's file
  and selected lines into the CLI prompt as `@src/file.py#L5-7`, like the VS Code extension. Bound
  in the `Workspace` context, so it also works while you are typing in the terminal dock (a terminal
  opened as a centre tab is itself the active item, and the shortcut says so). The action is
  `claude_code_ide::MentionSelection` if you want another key.
- Model-visible **diagnostics** (`getDiagnostics`), including the CLI's own
  post-edit check that feeds new errors back to the model.
- Open editors / open file / save / dirty checks
  (`getOpenEditors`, `openFile`, `saveDocument`, `checkDocumentDirty`).
- **Blocking accept/reject diffs** (`openDiff`): Claude's proposed change opens
  as a side-by-side diff tab in the centre pane, titled as the CLI named it,
  without taking focus from the terminal; the view centers on the first change
  and the CLI blocks until you click **Keep** (green) or **Reject** (red).
  Every other way out — dismissing the notification, closing the tab, `Esc`
  in the CLI, the CLI exiting or dying — counts as Reject and removes both the
  tab and the notification. Other tool calls keep being served meanwhile.

## How it works

1. Each *local* workspace starts a per-window server: it binds `127.0.0.1:0`,
   then writes a lockfile to `~/.claude/ide/<port>.lock` (honoring
   `CLAUDE_CONFIG_DIR`) with `0600` perms inside a `0700` dir. The lockfile
   advertises `{pid, workspaceFolders, ideName: "Zed", transport: "ws",
   runningInWindows, authToken}` so the CLI can discover the IDE, and is
   rewritten when folders are added to or removed from the project, because
   the CLI matches its working directory against that list. SSH, WSL and
   collab windows get no server: the port is on this machine's loopback and
   their buffers are not here.
2. The transport is a WebSocket authenticated by the
   `x-claude-code-ide-authorization` header (which must equal `authToken`),
   speaking JSON-RPC 2.0 / MCP (`initialize`, `tools/list`, `tools/call`),
   protocol version `2024-11-05`.
3. Auto-connect: the workspace publishes the server port onto `Project`, and
   `crates/project/src/terminals.rs` injects `CLAUDE_CODE_SSE_PORT` and
   `ENABLE_IDE_INTEGRATION=true` into *local* integrated terminals, so `claude`
   connects automatically. Terminals running over SSH are left alone: the port
   is on this machine's loopback, so advertising it there would name the remote
   host. Either way an external terminal can still attach via `/ide`,
   discovering Zed from the lockfile.

Entry point: `claude_code_ide::init(cx)`, called from `crates/zed/src/main.rs`.

## Tools

| Tool | Purpose |
| --- | --- |
| `getCurrentSelection` / `getLatestSelection` | Active editor selection (for `@`-mentions). |
| `getWorkspaceFolders` | Visible worktree roots. |
| `getOpenEditors` | Open buffers with uri/label/language/dirty state. |
| `getDiagnostics` | Language diagnostics, all buffers or one uri. |
| `openFile` | Open a path, optionally selecting a line range. |
| `saveDocument` / `checkDocumentDirty` | Save a buffer / query its dirty state. |
| `openDiff` | Blocking side-by-side diff with Keep/Reject. |
| `close_tab` / `closeAllDiffTabs` | Reject one pending diff by `tab_name` / all of them; closes their tabs. |

The CLI itself (2.1.x) calls only `getDiagnostics`, `openDiff`, `close_tab` and
`closeAllDiffTabs`, and hides every other `mcp__ide__*` tool from the model;
the rest are served for parity with the official extensions and for other MCP
clients.

Two wire details the CLI is strict about: `getDiagnostics` must answer with a
single text block holding a JSON array of
`{uri, diagnostics: [{message, severity: "Error"|"Warning"|"Info"|"Hint",
range: {start: {line, character}, end: {…}}, source}]}` — 0-based, UTF-16
columns, and the `uri` echoed exactly as requested — and `selection_changed`
is pushed by the IDE, never requested, as
`{selection: {start, end}, text, filePath}`.

## Try it

```bash
cargo run --release -p zed   # Linux: run ./script/linux once for build deps
```

Open Zed's integrated terminal and run `claude` — it connects automatically.
Edit a file through Claude and a Keep/Reject diff opens in the editor.

## Limitations

- Keep/Reject hotkeys are intentionally omitted; use the notification buttons.

