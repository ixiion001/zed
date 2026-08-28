# Changelog

Releases of this fork. Tags are `cc-v<upstream Zed version>-<n>`, so `cc-v1.16.3-2` is the second
build of the fork against Zed 1.16.3. Every release is published as a prerelease first;
`/releases/latest` excludes prereleases, so nothing updates itself to a build that has not been
promoted.

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
