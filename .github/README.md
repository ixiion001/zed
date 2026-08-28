# Zed + Claude Code

**An unofficial build of [Zed](https://zed.dev) with native Claude Code IDE integration.**
Run the real `claude` CLI in Zed's integrated terminal and it connects on its own — your selection,
diagnostics and Keep/Reject diffs appear in the editor.
Prebuilt for Windows, macOS and Linux.
Zed's own code is barely touched: the integration is a self-contained crate plus an
**85-line graft**, which is what lets this build follow upstream Zed rather than drift away
from it.

> Not affiliated with, endorsed by, or supported by Zed Industries. This is a personal fork.
> For official Zed, go to [zed.dev](https://zed.dev) or
> [zed-industries/zed](https://github.com/zed-industries/zed).

## What it does

Zed already offers Claude Code as an agent in its agent panel. This is the other direction: the
`claude` CLI, running in Zed's terminal, drives the editor — the same protocol the official VS Code
and JetBrains extensions speak.

- **Auto-connect.** No `/ide` needed; the terminal is told where to find the editor.
- `@`-mention your current selection.
- Model-visible **diagnostics** from Zed's language servers.
- Open a file, save it, check whether it is dirty, list open editors.
- **Keep/Reject diffs.** A proposed change opens as a side-by-side tab and the CLI waits for you.

## Install

Download from [Releases](../../releases) — the [changelog](CHANGELOG.md) says what each one is
built on and what has been verified. Every release ships a `.sha256` beside each asset; verify
with `sha256sum -c <asset>.sha256` (`shasum -a 256 -c` on macOS). The binaries are **unsigned** — the
source is this repository at the commit named in the release, and can be rebuilt from it.

**Windows** — unzip into `%LOCALAPPDATA%\Programs\ZedDev` and run `zed.exe` from there.
`conpty.dll` and `OpenConsole.exe` must sit beside it or the integrated terminal will not open.
No installer, no admin rights.

**macOS (Apple silicon)** — open the `.dmg`, drag **Zed Dev.app** to `/Applications`, then clear the
quarantine flag or macOS will report it as damaged:

```sh
xattr -dr com.apple.quarantine "/Applications/Zed Dev.app"
```

**Linux (x86_64)** — ALSA is not bundled and the editor will not start without it:

```sh
sudo apt install libasound2t64          # libasound2 before Ubuntu 24.04
tar -xzf zed-claude-code-linux-x86_64.tar.gz -C ~/.local
ln -sf ~/.local/zed-dev.app/bin/zed ~/.local/bin/zed
```

Built on Ubuntu 24.04, so **glibc 2.39 is the floor** — verified to fail on Ubuntu 22.04 and
Debian 12.

## Updating

These are **dev-channel** builds, which switches Zed's own updater off deliberately: a
stable-channel build would download official Zed over the top and silently discard the patch. Use
the updater scripts instead.

```sh
script/update-zed.sh  [--pre] [--dry-run]     # macOS and Linux
script/update-zed.ps1 [-Pre]  [-DryRun]       # Windows
```

Both read `/releases/latest`, which excludes prereleases — so nothing updates itself to a build
nobody has looked at. `--pre` / `-Pre` tries one anyway; `--dry-run` / `-DryRun` previews. Mind the
spelling: PowerShell takes a single dash, and `--pre` there binds to the first positional parameter
instead of failing.

## Known limitations

- **No inline streaming diffs.** `openDiff` opens a side-by-side tab. That is what the IDE protocol
  offers, not a shortcoming of this build.
- **Unsigned binaries**, so Gatekeeper and SmartScreen will object the first time.
- **No auto-update**, by design — see above.
- **Tracks Zed stable, one or two releases behind.** A weekly job rebases onto each new upstream
  stable; see the [changelog](CHANGELOG.md) for what a given release is built on.
- **Linux is verified headlessly, not on real graphics hardware.** The editor starts and the
  integration connects under Xvfb with software rendering; the artifact checks out too — the binary
  reports the right channel and commit, its only unbundled dependency is `libasound2t64`, and the
  updater installs and self-detects. What is untested is a real GPU and a real desktop session.
  Reports from one are welcome.
- Best-effort support. [Issues](../../issues) are open.

## How it works

Each Zed window binds a loopback port and writes `~/.claude/ide/<port>.lock` describing itself. The
CLI finds it there, connects over a WebSocket authenticated by a token from that file, and speaks
JSON-RPC 2.0 / MCP. Zed's integrated terminal exports `CLAUDE_CODE_SSE_PORT` and
`ENABLE_IDE_INTEGRATION`, which is what makes the connection automatic.

The patch is small on purpose: one new crate, `crates/claude_code_ide/`, plus an 85-line graft into
existing Zed code. Keeping it that small is what makes tracking upstream cheap — moving from Zed
1.16.3 to 1.17.2, across 118 upstream commits, took a one-line fix. See
[`crates/claude_code_ide/README.md`](../crates/claude_code_ide/README.md).

You do not have to take that on trust. This compares official Zed against this fork — every
line of the difference, nothing else:

[`zed-industries/zed@v1.17.2 ... ixiion001:main-patched`](https://github.com/zed-industries/zed/compare/v1.17.2...ixiion001:main-patched)

To download the change rather than read it, append the format to that URL. GitHub generates both
on demand, so they are never out of date with the branch:

- [**`.diff`**](https://github.com/zed-industries/zed/compare/v1.17.2...ixiion001:main-patched.diff)
  — one flat diff, about 200 KB. `git apply` it to a checkout of upstream `v1.17.2` and you have
  this fork. Verified: it applies clean, and the graft measures 85 lines in the result.
- [**`.patch`**](https://github.com/zed-industries/zed/compare/v1.17.2...ixiion001:main-patched.patch)
  — the same change as its individual commits, authorship intact. Apply it with
  `git -c core.symlinks=false am`: without that flag it stops partway, on a commit that adds
  `CLAUDE.md` before its file mode was corrected, with `File name too long`.

The graft is the part that matters, and it is reproducible from a clone. The fork does not carry
upstream's tags, so name the base tag once — the commits themselves are already there, because this
branch is rebased directly onto it:

```sh
git remote add zed https://github.com/zed-industries/zed
git fetch zed tag v1.17.2
git diff --shortstat v1.17.2..main-patched -- \
  crates/project/ crates/zed/src/main.rs crates/zed/Cargo.toml Cargo.toml
#   5 files changed, 85 insertions(+)
```

`script/claude-ide-probe.py` checks a running editor end to end — lock file, handshake, MCP,
`tools/call` — and exits non-zero if anything regressed.

## Building it yourself

```sh
cargo build --release --package zed --package cli
```

macOS additionally needs full Xcode: `gpui` compiles Metal shaders with `xcrun -sdk macosx metal`,
which the Command Line Tools do not ship. Linux needs `./script/linux` first. Always build with the
`dev` channel — see the release-channel note in [`CLAUDE.md`](../CLAUDE.md), which is the single
easiest way to end up with a binary that quietly replaces itself.

## Credits and licence

- [Zed Industries](https://github.com/zed-industries/zed) — the editor.
- [vitaly-andr](https://github.com/vitaly-andr/zed) — the original `crates/claude_code_ide`
  integration, which is the substance of this fork.
- This fork — Windows discovery, handshake and path fixes (offered back upstream), and the
  cross-platform release pipeline.

GPL-3.0-or-later, as Zed is. The corresponding source is this repository, which stays public.
