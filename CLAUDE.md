# This fork — read this before touching anything

Zed's own Rust guidelines still apply and are imported here:

@.rules

Upstream ships `CLAUDE.md` as a *symlink* to `.rules`. This file replaces that symlink with a regular
file so fork-specific context can live alongside those rules — the import above preserves them. If you
ever re-add this file, make sure git records mode **100644**: a 120000 (symlink) entry holding this
much text makes `git checkout` fail with `ENAMETOOLONG` on macOS and Linux, which is invisible on
Windows because `core.symlinks=false` writes symlinks out as plain files.

This is **not** upstream Zed. It is `ixiion001/zed`, a fork carrying a Claude Code IDE integration
patch, being turned into a cross-platform distribution.

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
| `main-patched` | **default branch and the live patch series.** v1.16.3 + commits. CI builds this |
| `fix/windows-ide-integration` | PR #1 to `vitaly-andr/zed` — four upstream bug fixes. **Never add release infrastructure here**; it must stay reviewable |
| `windows-ide-fixes` | tagged `fallback-1.6.0`, the known-good Zed 1.6.0 lineage. Left alone |
| `auto/v<tag>` | created by the weekly auto-rebase job for inspection |

## Five traps that cost real time

1. **Release channel.** `crates/zed/RELEASE_CHANNEL` says `stable`, and `script/bundle-mac:54` /
   `bundle-windows.ps1:61` export it as `ZED_RELEASE_CHANNEL`. A release build uses that compile-time
   value with no runtime override — so a bundle-script build ships a **stable-channel binary with
   auto-update live**, which downloads official Zed over itself and silently deletes the patch.
   Always build with `ZED_RELEASE_CHANNEL=dev`; verify the result is stamped `+dev.`.
2. **File modes on Windows.** `core.symlinks=false` makes symlinked files look like ordinary ones. Check
   `git ls-files -s <path>` before committing anything that upstream may ship as a symlink — see the
   note at the top of this file, which is exactly the mistake that produced it.
3. **Do not run `cargo fmt` across `crates/claude_code_ide/`.** It is not rustfmt-clean, so a
   repo-wide run reformats ~137 lines of the original author's code and buries the real change.
4. **Windows long paths.** Zed's `pet` git dependency has fixtures exceeding `MAX_PATH`; cargo's
   vendored libgit2 cannot check them out. Needs `core.longpaths` +
   `CARGO_NET_GIT_FETCH_WITH_CLI=true`.
5. **You cannot overwrite a running `.exe`.** Rename `zed.exe` aside before rebuilding while Zed is
   open — renaming a running binary is permitted on Windows, overwriting is not.

## Where the plan lives

`docs/claude-code/plan.md` is the authoritative record: tasklist with gates, decisions, pinned
versions. `docs/claude-code/build-history.md` carries the root-cause notes for the four Windows bugs
fixed here. Read them before proposing work — most obvious questions are answered there, with evidence.

## Coordination — two sessions are working this repo

| Session | Machine | Owns |
|---|---|---|
| Windows | `<windows-workspace>` | `main-patched`, CI, Gate 1, the release workflow |
| macOS (`macos-host`) | Apple silicon | the macOS leg: toolchain, `script/bundle-mac`, producing a known-good recipe |

Neither session creates branches, commits, pushes or dispatches workflows without the maintainer coordinating
it — two agents force-pushing one branch is an expensive way to lose an afternoon. Propose diffs and
let him relay. **Agent-to-agent `SendMessage` does not work across independent Claude Code sessions**
(separate session trees, no shared registry), so relaying happens through the maintainer or through commits
to this repo.
