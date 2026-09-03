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

`crates/claude_code_ide/` (~1900 lines of Rust; ~1360 as **vitaly-andr** first wrote it) runs a
per-window WebSocket server so the unmodified `claude` CLI connects to Zed the way it connects to VS
Code — discovery via `~/.claude/ide/<port>.lock`, transport JSON-RPC 2.0 / MCP. Plus a **43-line
graft** into existing Zed code: `crates/project/src/project.rs` stores the bound port (18 lines),
`crates/project/src/terminals.rs` injects `CLAUDE_CODE_SSE_PORT` + `ENABLE_IDE_INTEGRATION` into
local integrated terminals inside `resolve_directory_environment`, the one function all three spawn
paths call (21 lines), `crates/zed/src/main.rs` calls `claude_code_ide::init(cx)` (1 line), and two
`Cargo.toml` entries.

That graft is the entire long-term maintenance surface, and it is the number to watch. It was 46
lines when the crate arrived; `95bc78c320` took it to 85 by pasting the remote-terminal check into
all three spawn paths; the review-fixes series brought it back to 43 by injecting once, one level
down. The direction is one-way unless someone is counting.

Do not trust the figure above — it drifted from 43 to 85 while still being quoted as 43. Measure it:

```sh
git diff --shortstat v<base>..HEAD -- crates/project/ crates/zed/src/main.rs crates/zed/Cargo.toml Cargo.toml
```

Small is the whole point: rebasing 1.16.3 → 1.17.2, across 118 upstream commits, cost exactly **one
line** of conflict resolution — upstream's `chrono` landing next to our `claude_code_ide` in
`crates/zed/Cargo.toml`.

## Branches

| Branch | Role |
|---|---|
| `main-patched` | **default branch and the live patch series.** v1.16.3 + commits. CI builds this |
| `fix/windows-ide-integration` | PR #1 to `vitaly-andr/zed` — four upstream bug fixes. **Never add release infrastructure here**; it must stay reviewable |
| `windows-ide-fixes` | tagged `fallback-1.6.0`, the known-good Zed 1.6.0 lineage. Left alone |
| `auto/v<tag>` | created by the weekly auto-rebase job for inspection |

## CI layout

| File | Whose |
|---|---|
| `.github/workflows/cc-release.yml` | **ours.** Builds on `cc-v*` tags, publishes a prerelease |
| `.github/workflows/claude-code-ide.yml` | **ours.** Tests the crate on three platforms |
| `.github/workflows/auto-rebase.yml` | **ours.** Weekly: rebases onto each new upstream stable, checks and tests it, then opens an issue. Never publishes — see the note in its header on why a `GITHUB_TOKEN` tag push cannot start `cc-release` |
| `.github/workflows/release.yml` and every other file there | **upstream's — leave alone** |

Releases are tagged `cc-v<upstream>-<n>`, e.g. `cc-v1.16.3-1`. Push tags to **`fork`**; `origin` is
`vitaly-andr/zed` and `git push --tags` would put them on someone else's repository.

## Traps that cost real time

1. **Release channel — the file wins, and `ZED_RELEASE_CHANNEL=dev` alone does not work.** A
   stable-channel build has auto-update live and downloads official Zed over itself, silently
   deleting the patch. Two mechanisms decide the channel and only one reads the environment:
   `crates/release_channel/build.rs` bakes in `$ZED_RELEASE_CHANNEL` **only if it is set at build
   time**, else `include_str!` of `crates/zed/RELEASE_CHANNEL`; and every bundle script
   (`bundle-mac:52-54`, `bundle-windows.ps1:61`, `bundle-linux`) reads that file and **exports its
   contents over the environment**. So `ZED_RELEASE_CHANNEL=dev script/bundle-mac` bakes in whatever
   the file says. Plain `cargo build` is the only path the env var governs, which is why the Windows
   leg is safe. ⇒ **`echo dev > crates/zed/RELEASE_CHANNEL` before any bundle script** — what the
   Linux and macOS legs do. Verify the artifact, never the intent: `+dev.` in the Windows version
   string, `zed-dev.app/` in the Linux tarball, `CFBundleIdentifier == dev.zed.Zed-Dev` on macOS.
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
6. **Never put fork CI at a path upstream owns.** Our release workflow originally *replaced* Zed's
   1029-line `.github/workflows/release.yml`. Upstream regenerates that file from
   `xtask::workflows::release`, so every rebase over it conflicted — which would have stopped the
   weekly auto-rebase dead, every week, for nothing. Ours is `cc-release.yml` now. Upstream's copy is
   inert in a fork anyway: its jobs are guarded by `repository_owner == 'zed-industries'`.
7. **Two ways a publish job dies *after* a two-hour build.** Both were caught before that happened,
   both cost nothing to prevent:
   - a job that skips `actions/checkout` has no git remote, so `gh` aborts with
     `fatal: not a git repository`. Set `GH_REPO: ${{ github.repository }}`.
   - naming *any* scope in a `permissions:` block sets every unnamed scope to `none`. Spelling out
     `contents: write` alone left the job unable to read its own run's artifacts; `actions: read` was
     needed too.

   (The repository's Actions default is read-only, but a job requesting `contents: write` is still
   granted it — verified, no settings change required.)
8. **PowerShell writes CRLF, which breaks checksums on Unix.** `Out-File` gave the `.sha256` file
   CRLF endings, so `sha256sum -c` on Linux and macOS looked for a filename with a trailing carriage
   return: `'…zip'$'\r': No such file or directory`. That breaks the verification step *and* the
   instruction given to users. Write such files through .NET with an explicit newline instead:

   ```powershell
   [System.IO.File]::WriteAllText("$PWD/$name.zip.sha256", "$hash  $name.zip`n")
   ```
9. **`script/bundle-mac -i` is broken for release builds.** Local-install moves the `.app` to
   `/Applications` at line 229, then the DMG branch `mv`s a path that no longer exists and `set -e`
   aborts. Only `-d` (debug) skips the DMG. Note also that the dev-channel bundle is **`Zed Dev.app`**
   / `dev.zed.Zed-Dev`, not `Zed.app` — it coexists with official Zed and keeps its own settings.

## Building

macOS needs **full Xcode**, not just Command Line Tools: `crates/gpui_macos/build.rs:132` compiles the
Metal shaders with `xcrun -sdk macosx metal`, which CLT does not ship. The Mac has CLT only and no room
for Xcode, so **macOS builds in CI**, where the runner has Xcode, cmake and Node preinstalled. Linux
builds in CI too.

Windows is therefore the only machine that can build locally — worth remembering before step F reclaims
its toolchain. Running the editor needs no toolchain at all, which is why the Mac and Linux boxes test
artifacts rather than producing them.

## Working on this repo

More than one agent session may be working here at once, on different machines. None of them creates
branches, commits, pushes or dispatches workflows without the maintainer coordinating it — two agents
force-pushing one branch is an expensive way to lose an afternoon. Propose diffs and let the
maintainer relay them. **Agent-to-agent `SendMessage` does not work across independent Claude Code
sessions** (separate session trees, no shared registry), so relaying happens through the maintainer or
through commits to this repo.

Planning notes, the tasklist and the root-cause write-ups for the four Windows bugs are kept outside
this repository. Ask the maintainer for them rather than assuming an undocumented decision was
arbitrary.
