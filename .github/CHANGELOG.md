# Changelog

Releases of this fork. Tags are `cc-v<upstream Zed version>-<n>`, so `cc-v1.16.3-2` is the second
build of the fork against Zed 1.16.3. Every release is published as a prerelease first;
`/releases/latest` excludes prereleases, so nothing updates itself to a build that has not been
promoted.

## Unreleased — `main-patched` since cc-v1.17.2-1

Fifteen findings from a code review of the patch, several verified against the CLI binary
(2.1.258). Two of them mean earlier builds did less than they appeared to.

### Fixed

- `getDiagnostics` answers in the shape the CLI parses: one text block holding a JSON array of
  `{uri, diagnostics: [{message, severity, range, source}]}`, 0-based, severity as a name, the
  requested `uri` echoed verbatim. Before, the CLI's post-edit diagnostics check failed silently on
  every buffer that had a diagnostic. Windows paths now compare case-insensitively, so a request
  spelled `c:\proj\SRC` finds the buffer Zed knows as `C:\proj\src`.
- Zed pushes `selection_changed` as the active editor's selection moves. That is the only way the
  CLI learns the active file and selection: its footer ("In file.py", "N lines selected") and the
  selection it hands the model both come from it. Before, nothing was ever pushed.
- Every way out of a Keep/Reject diff settles the request: closing the tab, the toast's close
  button, `close_tab` on Esc or exit, `closeAllDiffTabs`, or the CLI dying. Before, the tab and the
  toast could outlive the request, and a later Keep answered with contents nobody could see.
- The diff opens in the centre pane, titled as the CLI named it, without taking focus from the
  terminal. Before, every proposal split the layout, because the pane heuristic never saw the dock
  the terminal lives in.
- A `CLAUDE_CODE_SSE_PORT` inherited from the shell Zed was started in no longer wins over the
  window's own port.
- SSH, WSL and collab windows no longer start a server or advertise remote paths in a local lock file.
- The lock file follows Add/Remove Folder, so discovery by working directory stays accurate.
- Connections end with their window instead of answering "entity released" to a CLI that still
  believed it was connected; a transient accept failure no longer tears the server down.

### Changed

- The graft into existing Zed code is 43 lines, down from 85: the terminal port injection lives
  once, in the function all three spawn paths call.

### Verified

Crate tests locally (17) and on Linux, macOS and Windows CI; the built Windows editor against
`claude` 2.1.258/259: automated wire probe 19/19, then Keep, tab close, connection death, Add Folder,
second window, footer and `<new-diagnostics>` by hand.

## cc-v1.17.2-1 — published 2026-08-28, promoted 2026-08-31

Zed **1.17.2** plus `crates/claude_code_ide`, built from `d27e2bc1cf`.

### Changed

- Rebased from Zed 1.16.3 to 1.17.2, across 118 upstream commits. Cost: one
  conflict line — upstream's `chrono` landing beside `claude_code_ide` in
  `crates/zed/Cargo.toml`.

### Added

- The source delta ships with the release: `zed-claude-code-cc-v1.17.2-1.diff.gz`
  is everything this build changes in upstream Zed and nothing else. `gunzip` it
  and `git apply` it to a checkout of upstream `v1.17.2` to rebuild from source.

### Verified

| | build | channel guard | install | integration |
|---|---|---|---|---|
| macOS | 2 h 10 m | `dev.zed.Zed-Dev` | `update-zed.sh --pre` | **31/31** protocol checks against the shipped build |
| Linux | 1 h 02 m | binary executed in CI: `Zed dev 1.17.2 d27e2bc1cf…` | — | — |
| Windows | 1 h 48 m | `1.17.2+dev.5.d27e2bc1cf…` | pending | pending |

Every leg's guard now proves the artifact itself is a dev-channel build of the
tagged commit before anything publishes — a stable-channel build would let Zed's
own updater silently replace the patch, and can no longer ship. Promotion was by
hand this time, gated on the macOS install and probe; from the next release it
happens in CI once all three guards pass.

## cc-v1.16.3-2 — 2026-08-28

Zed 1.16.3 plus `crates/claude_code_ide`, built from `95c83f3ada`.

**The first release built and verified on all three platforms.** Until now only the Windows archive
was published.

### Added

- **macOS (Apple silicon)** — a `.dmg`, built by `script/bundle-mac` unmodified, the same script
  upstream's own release workflow runs.
- **Linux (x86_64)** — a `.tar.gz` from `script/bundle-linux`, with the `libstdc++` and other `ldd`
  dependencies bundled beside the binary.
- `script/update-zed.sh` — the macOS and Linux counterpart of `update-zed.ps1`. Reads
  `/releases/latest`, verifies the `.sha256`, identifies the install by commit rather than version,
  and renames the running bundle aside rather than overwriting it.
- macOS bundles carry a `ZedCommitSha` key in `Info.plist`, so the updater can tell which build is
  installed without executing it.
- Asset names unified to `zed-claude-code-<os>-<arch>`. The Linux tarball had been
  `zed-linux-x86_64.tar.gz`, byte-for-byte the name upstream's own Linux build produces — which
  made this fork's artifact indistinguishable from official Zed in a downloads folder.

### Fixed

In the integration:

- Windows discovery, handshake and path handling — four separate defects, any one of which made the
  integration unusable there. Offered upstream as
  [vitaly-andr/zed#1](https://github.com/vitaly-andr/zed/pull/1).
- `openDiff` no longer hangs when the notification is dismissed.
- Tool calls are served concurrently rather than one at a time.
- The IDE port is withdrawn when the server stops, so a stale lock file is not left for the CLI to
  find.
- A diff against a base that could not be read is refused rather than silently computed against
  nothing.
- The server keeps serving after a failed `accept` instead of stopping.
- The port is advertised only to local terminals.
- URI and position handling now follow the protocol's conventions, with tests covering the escaping.

In the tooling:

- The updater no longer deletes the bundle it moved aside, and no longer breaks a running install.
- Wrong-architecture downloads are refused instead of quietly doing nothing.
- A missing repository is reported differently from one with nothing promoted yet.
- The DMG licence prompt is answered when mounting, which previously hung the updater.

### Verified

| | build | install | integration |
|---|---|---|---|
| Windows | 1 h 20 m | `update-zed.ps1`, with the editor open | connected |
| macOS | 2 h 11 m | `update-zed.sh` | 29/29 protocol checks |
| Linux | 1 h 18 m | `update-zed.sh`, unprivileged, in `ubuntu:24.04` | connected, headless under Xvfb |

The Linux binary was executed (`bin/zed --version` reports `Zed dev 1.16.3 95c83f3ada…`, confirming
the dev channel is compiled in), its dependencies resolved on a stock Ubuntu 24.04 image with only
`libasound2t64` added, and the updater run end to end. The editor itself is verified headlessly,
under Xvfb with software rendering — not on real graphics hardware; see the limitations in the
[README](README.md).

## cc-v1.16.3-1

Windows only, published as a prerelease and never promoted. Superseded by `cc-v1.16.3-2` and no
longer available.
