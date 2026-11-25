# This fork — read this before touching anything

Zed's own Rust guidelines still apply and are imported here:

@.rules

Upstream ships `CLAUDE.md` as a *symlink* to `.rules`. This file replaces that symlink with a regular
file so fork-specific context can live alongside those rules — the import above preserves them. If you
ever re-add this file, make sure git records mode **100644**: a 120000 (symlink) entry holding this
much text makes `git checkout` fail with `ENAMETOOLONG` on macOS and Linux, which is invisible on
Windows because `core.symlinks=false` writes symlinks out as plain files.

This is **not** upstream Zed. It is `ixiion001/zed`, a fork carrying a Claude Code and Codex IDE
integration patch on top of each upstream stable release, built and released weekly for Windows, Linux
and macOS.

## What the patch is

`crates/claude_code_ide/` (about 3,900 lines of Rust; ~1360 as **vitaly-andr** first wrote it) holds
two providers:

- **Claude Code.** A per-window WebSocket server so the unmodified `claude` CLI connects to Zed the
  way it connects to VS Code — discovery via `~/.claude/ide/<port>.lock`, transport JSON-RPC 2.0 / MCP.
  See the crate's `README.md`.
- **Codex** (`src/codex.rs`, `src/codex/`, experimental). Answers the unmodified Codex CLI's `/ide`
  context requests over a Unix socket or a Windows named pipe. See the crate's `CODEX.md` for what
  has and has not been accepted on each platform before changing or claiming anything about it.

Plus a **43-line graft** into existing Zed code: `crates/project/src/project.rs` stores the bound
port (18 lines), `crates/project/src/terminals.rs` injects `CLAUDE_CODE_SSE_PORT` +
`ENABLE_IDE_INTEGRATION` into local integrated terminals inside `resolve_directory_environment`, the
one function all three spawn paths call (21 lines), `crates/zed/src/main.rs` calls
`claude_code_ide::init(cx)` (1 line), and three `Cargo.toml` lines. The three default keymaps carry
nine more lines for the `@`-mention shortcut.

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
`crates/zed/Cargo.toml` — and 1.26.2 → 1.29.3 cost none. When a rebase does conflict, it is in the
root `Cargo.toml` or the keymaps, where upstream adds a line beside ours; keep both.

The crate is also built on its own (`cargo test -p claude_code_ide`, here and in CI), without the
feature unification an editor build gets. It must name every feature it uses itself: upstream dropped
`Win32_System_Pipes` from the workspace `windows` features, the editor still built, and the crate's
own build did not.

## Branches

| Branch | Role |
|---|---|
| `main-patched` | **default branch and the live patch series**: the newest upstream stable tag plus our commits. The weekly job rebases and force-moves it; releases are tagged from it. **Everything lands here** — the job reads nothing else. Find its base with `git describe --tags --match 'v*' --abbrev=0` after fetching upstream's tags |
| `fix/windows-ide-integration` | PR #2 to `vitaly-andr/zed` — four upstream bug fixes, on Zed 1.6.0. **Never add release infrastructure here**; it must stay reviewable |
| `auto/v<tag>` | the series as it last stood on that upstream tag, archived by the weekly auto-rebase job when it moves `main-patched` on. Scratch space: **the job never reads these**, so a fix parked here is invisible to it |

## CI layout

| File | Whose |
|---|---|
| `.github/workflows/cc-release.yml` | **ours.** Builds all three platforms on `cc-v*` tags (3–4 h, macOS the long pole). Publishes only if every leg and its channel guard is green, then promotes the release to `/releases/latest` |
| `.github/workflows/claude-code-ide.yml` | **ours.** Tests the crate on three platforms |
| `.github/workflows/auto-rebase.yml` | **ours.** Weekly: rebases onto each new upstream stable, checks and tests it, **force-moves `main-patched` to the result** and tags it, which starts `cc-release`. Also re-tags a `main-patched` that is current but has no release, **and one whose source has changed since its last release** — so whatever is merged into `main-patched` ships within a week, and unfinished work stays on a branch (changes confined to `.github/`, `*.md` and `script/update-zed.*` do not trigger a release). Opens an issue only when a human is needed. After a week with a new upstream release, every clone recovers with `git fetch && git reset --hard <remote>/main-patched`, never `git pull` |
| `.github/workflows/heartbeat.yml` | **ours.** Does nothing; an external check dispatches it to detect GitHub switching Actions off on the fork. Keep it enabled |
| `.github/workflows/release.yml` and every other file there | **upstream's — leave alone.** They are disabled in this fork, and auto-rebase disables any new one upstream adds |

Releases are tagged `cc-v<upstream>-<n>`, e.g. `cc-v1.29.3-1`; `n` counts tags for that upstream
version, so a retry never reuses a number. A fix that cannot wait for Monday is released by pushing
the next tag by hand. Push to the remote that points at `ixiion001/zed` — it is
`fork` on the Windows clone, where `origin` is `vitaly-andr/zed` and `git push --tags` would put our
tags on someone else's repository.

A conflict the weekly job reports is resolved by rebasing `main-patched` itself onto the new tag and
force-pushing it with a lease. The next run finds it current but unreleased, tests it and tags it.

In a checkout with a remote named `upstream`, `gh` addresses `zed-industries/zed` by default. Pass
`--repo ixiion001/zed` (or set `GH_REPO`) to every `gh run`, `gh issue` and `gh release` command.

## Traps that cost real time

1. **Release channel — the file wins, and `ZED_RELEASE_CHANNEL=dev` alone does not work.** A
   stable-channel build has auto-update live and downloads official Zed over itself, silently
   deleting the patch. Two mechanisms decide the channel and only one reads the environment:
   `crates/release_channel/build.rs` bakes in `$ZED_RELEASE_CHANNEL` **only if it is set at build
   time**, else `include_str!` of `crates/zed/RELEASE_CHANNEL`; and every bundle script
   (`bundle-mac`, `bundle-windows.ps1`, `bundle-linux`) reads that file and **exports its
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
   `/Applications`, then the DMG branch `mv`s a path that no longer exists and `set -e`
   aborts. Only `-d` (debug) skips the DMG. Note also that the dev-channel bundle is **`Zed Dev.app`**
   / `dev.zed.Zed-Dev`, not `Zed.app` — it coexists with official Zed and keeps its own settings.

## Building

macOS needs **full Xcode**, not just Command Line Tools: `crates/gpui_apple/build.rs` compiles the
Metal shaders with `xcrun -sdk macosx metal`, which CLT does not ship. The Mac has CLT only and no room
for Xcode, so **macOS builds in CI**, where the runner has Xcode, cmake and Node preinstalled. Linux
builds in CI too.

Windows is therefore the only machine that can build locally. Running the editor needs no toolchain
at all, which is why the Mac and Linux boxes test artifacts rather than producing them. To install a
release, use `script/update-zed.sh` or `script/update-zed.ps1` (see `.github/README.md`).

## Working on this repo

More than one agent session may be working here at once, on different machines, and the weekly job
force-moves `main-patched` on its own. None of the sessions creates branches, commits, pushes or
dispatches workflows without the maintainer coordinating it — two agents force-pushing one branch is
an expensive way to lose an afternoon. Before any push, compare with the remote
(`git ls-remote <remote> main-patched`); if `main-patched` has moved under you, `git fetch` and
`git reset --hard <remote>/main-patched`, then rebase your work onto it. Never `git pull` it, and
lease every force-push on the commit you last saw. Propose diffs and let the
maintainer relay them. **Agent-to-agent `SendMessage` does not work across independent Claude Code
sessions** (separate session trees, no shared registry), so relaying happens through the maintainer or
through commits to this repo.

Planning notes, the tasklist and the root-cause write-ups for the four Windows bugs are kept outside
this repository. Ask the maintainer for them rather than assuming an undocumented decision was
arbitrary.
