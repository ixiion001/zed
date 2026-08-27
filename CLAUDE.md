.rules

# This fork — read this before touching anything

This is **not** upstream Zed. It is `ixiion001/zed`, a fork carrying a Claude Code IDE integration
patch, being turned into a cross-platform distribution. Zed's own Rust guidelines still apply — see
`.rules`, referenced above — but the repository layout and workflow below are specific to this fork.

## What the patch is

`crates/claude_code_ide/` (~1460 lines, originally by **vitaly-andr**) runs a per-window WebSocket
server so the unmodified `claude` CLI connects to Zed the way it connects to VS Code — discovery via
`~/.claude/ide/<port>.lock`, transport JSON-RPC 2.0 / MCP. Plus a **43-line graft** into existing Zed
code: `crates/project/src/project.rs` stores the bound port, `crates/project/src/terminals.rs` injects
`CLAUDE_CODE_SSE_PORT` + `ENABLE_IDE_INTEGRATION` into integrated terminals (three call sites), and
`crates/zed/src/main.rs` calls `claude_code_ide::init(cx)`.

That 43-line graft is the entire long-term maintenance surface. Keep it that small — it is why
rebasing onto a new Zed release costs ~10 lines rather than a day.

## Branches

| Branch | Role |
|---|---|
| `main-patched` | **default branch and the live patch series.** v1.16.3 + 8 commits. CI builds this |
| `fix/windows-ide-integration` | PR #1 to `vitaly-andr/zed` — four upstream bug fixes. **Never add release infrastructure here**; it must stay reviewable |
| `windows-ide-fixes` | tagged `fallback-1.6.0`, the known-good Zed 1.6.0 lineage. Left alone |
| `auto/v<tag>` | created by the weekly auto-rebase job for inspection |

## Four traps that cost real time

1. **Release channel.** `crates/zed/RELEASE_CHANNEL` says `stable`, and `script/bundle-mac:54` /
   `bundle-windows.ps1:61` export it as `ZED_RELEASE_CHANNEL`. A release build uses that compile-time
   value with no runtime override — so a bundle-script build ships a **stable-channel binary with
   auto-update live**, which downloads official Zed over itself and silently deletes the patch.
   Always build with `ZED_RELEASE_CHANNEL=dev`; verify the result is stamped `+dev.`.
2. **Do not run `cargo fmt` across `crates/claude_code_ide/`.** It is not rustfmt-clean, so a
   repo-wide run reformats ~137 lines of the original author's code and buries the real change.
3. **Windows long paths.** Zed's `pet` git dependency has fixtures exceeding `MAX_PATH`; cargo's
   vendored libgit2 cannot check them out. Needs `core.longpaths` +
   `CARGO_NET_GIT_FETCH_WITH_CLI=true`.
4. **You cannot overwrite a running `.exe`.** Rename `zed.exe` aside before rebuilding while Zed is
   open — renaming a running binary is permitted on Windows, overwriting is not.

## Where the plan lives

`docs/claude-code/plan.md` in this repo is the authoritative record: tasklist with gates, decisions,
pinned versions, and root-cause notes for the four Windows bugs fixed here. Read it before proposing
work — most obvious questions are already answered there, usually with evidence.

## Coordination

**Another Claude Code session is actively working this repo** from
`<windows-workspace>`, with builds and CI in flight against `main-patched`.

Treat this clone as **orientation and review, not a place to commit**. Before creating branches,
committing, pushing, or dispatching workflows, say what you intend and let the human coordinate — two
agents force-pushing the same branch is an expensive way to lose an afternoon. Reading, building
locally, and proposing diffs are all fine.
