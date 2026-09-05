# Native Codex IDE integration

Implementation is in the existing `claude_code_ide` crate and startup hook. This
is an implementation under acceptance testing, not a cross-platform release claim.

Run the unmodified Codex CLI in a local project opened in Zed. `/ide on` enables
context, `/ide off` disables it, and `/ide status` reports that CLI's state. The
provider does not enable context or add prompt text itself. Codex requests a fresh
snapshot before each prompt when enabled.

Zed's command palette action **codex: show status** reports provider registration,
the last successfully answered context request, and the last error. Registration
is not a count of terminals and does not mean `/ide` is enabled.

## Pinned protocol evidence

- CLI **0.153.4**: [IPC client](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/tui/src/ide_context/ipc.rs),
  [context types](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/tui/src/ide_context.rs),
  [command behavior](https://github.com/openai/codex/blob/rust-v0.153.4/codex-rs/tui/src/chatwidget/ide_context.rs).
- Official Marketplace extension **26.5901.22334**:
  [openai.chatgpt](https://marketplace.visualstudio.com/items?itemName=openai.chatgpt).
  The downloaded VSIX was inspected, not installed or included in this repository.
- [Source hashes](tests/fixtures/codex/source-sha256.json) identify the inspected
  artifacts. [Exchange fixtures](tests/fixtures/codex/exchanges.json) record the
  expected messages, using artificial IDs, paths and selections.

Each frame is a four-byte little-endian unsigned byte length followed by one
UTF-8 JSON object. Frames cannot be empty or exceed 256 MiB. A connection can carry
several frames; partial headers and bodies are supported. Partial frames and
writes have a four-second deadline. Idle registered providers need not send
heartbeats. The CLI's whole-request budget is five seconds.

The CLI sends an unregistered `request` with `sourceClientId: "codex-tui"`,
`method: "ide-context"`, `version: 0`, and `params.workspaceRoot`. The provider
registers with `initialize` and `params.clientType: "zed"`; the router returns its
assigned `clientId`. Discovery wraps the original request in a
`client-discovery-request`, answered with `response.canHandle`. Successful context
responses use `result.ideContext`, `method`, and `handledByClientId`. Errors use
`resultType: "error"` and an error string. There is no JSON-RPC/MCP envelope here.

## Endpoint ownership and coexistence

| Platform | Primary endpoint |
| --- | --- |
| macOS / Linux | `<CODEX_HOME>/ipc/ipc.sock` |
| Native Windows | `\\.\pipe\codex-ipc` |

`CODEX_HOME` defaults to the user's `.codex` directory. A custom absolute
`CODEX_HOME` must be in **Zed's launch environment** as well as the CLI's; changing
it only inside a terminal cannot move the running application's service. Windows
uses the fixed native pipe regardless of `CODEX_HOME`.

On Unix, discovery also tries the CLI's legacy temporary socket names. Directories
must be owned by the current user and not writable by other users; symlink endpoints
and non-sockets are refused. New directories use 0700 and sockets use 0600. Both
connections and accepted peers verify the effective user. A Zed election lock
serializes Zed processes. Stale socket removal requires connection refusal and an
unchanged device/inode; permission failures never authorize removal. Cleanup only
removes the listener's own inode. Both the provider connection and any Zed-owned
listener monitor their endpoint identity. If another router replaces the pathname,
Zed closes the old connection and reconnects even if the old router process is
still alive. The official router does not participate in
Zed's election lock, so simultaneous election against the official app remains a
native acceptance gate, including the stale-socket race.

Windows creates a protected DACL granting access only to the current user SID,
rejects remote pipe clients, verifies peer process token ownership on both sides,
and uses `FILE_FLAG_FIRST_PIPE_INSTANCE` to arbitrate ownership. Busy or inaccessible
pipes cause a retry, not creation of another router beneath the owner.

If a live endpoint accepts a connection, Zed joins it. A failed registration does
not authorize replacing that endpoint. With no router, Zed hosts registration,
discovery, forwarding, broadcasts (including targeted broadcasts and client status),
and disconnect handling. Other application methods are forwarded unchanged; Zed's
provider only implements `ide-context`. Forwarded IDs are isolated from caller IDs,
and responses are accepted only from the selected peer. Slow peers are disconnected
rather than accumulating an unbounded queue. Owner exit triggers rediscovery after
one second; all service tasks use Zed's existing Tokio runtime.

## Workspace and editor boundaries

Requests carry a directory but no terminal or Zed-window identity. The most specific
containing workspace root wins; equally specific roots in different Zed windows
are rejected. Matching normalizes components and existing symlink aliases; Windows
also normalizes drive casing and separators. Root changes are read on each request.

**Use one context-providing editor per project across Zed, VS Code and Cursor.**
A Zed-owned router rejects multiple willing IDE providers. An official-owned router
uses its own first-willing-provider policy; this protocol does not let Zed enforce
global ambiguity rejection there. Untargeted IDE discovery waits for all registered
providers to answer; an unresponsive provider causes a deadline error, rather than
an uncertain choice.

Only local desktop projects participate. WSL, SSH, containers and collaboration
clients are outside this slice. A context read returns native active-file and tab
descriptors, zero-based UTF-16 selection ranges, and live selected text (including
unsaved edits). Absolute native descriptor paths also work when the CLI runs below
the workspace root. The last eligible singleton editor is remembered while a
terminal or another item is active; it must still be an open workspace item.
Untitled, remote and multi-buffer editors do not provide file context. With no
eligible editor, `activeFile` is null; a matching empty workspace still returns an
empty context.

Selection text is bounded to 200,000 UTF-8 bytes, without splitting a character.
This is intentionally a conservative bound relative to the extension's 200,000
UTF-16-unit limit. There are at most 128 tab descriptors and 1,024 selection ranges.
No full-buffer reads, diagnostics, navigation or diff approval are exposed through
this protocol. Claude retains its existing 32 KiB limit, truncation notice, JSON,
authentication, notifications, terminal environment and Keep/Reject workflow. Only
local file identity, UTF-16 conversion and bounded rope text copying are shared.
Universal shortcut routing remains a separate slice.

## Verification and remaining acceptance gates

Run `cargo check -p claude_code_ide` and `cargo test -p claude_code_ide` from the
repository root. Native socket tests require permission to bind local IPC endpoints.
Tests use isolated temporary endpoints, never the user's actual legacy socket.
The Windows pipe test uses a unique test-only pipe name.

Verified on the implementation host (macOS, 2026-09-05):

- Full crate compilation with the pinned **Rust 1.97.1** toolchain and **36 passing unit tests**
  with Rust 1.98.1, including existing Claude protocol/path/
  lock-file regressions, live unsaved buffer text and UTF-16 coordinates.
- Framing, malformed input, limits, discovery, request ID isolation, targeted
  broadcasts, disconnects, deadlines, workspace matching and duplicate rejection.
- Real Unix socket ownership/permissions, stale recovery, refusal of unsafe paths,
  provider registration, successive fresh requests, router-owner exit recovery,
  and replacement of a socket pathname while its old router remains alive.
- A temporary interoperability harness ran the **production CLI 0.153.4 IPC code**
  against the **router class extracted unchanged from extension 26.5901.22334**,
  then killed that router and repeated the request through Zed's replacement router.
  Artificial context was returned successfully in both cases. This checks transport
  interoperability, not a running CLI TUI or Zed UI. The harness supplied Node framing
  and logging around the extracted router class; the complete VS Code extension was
  not launched.
- Standalone compilation of the actual transport/service/router modules for native
  Windows and Linux targets. Cross-compilation is not native runtime verification.

Still required before a release claim on **each native platform**:

- Unmodified CLI `/ide on`, `/ide off`, `/ide status` and real prompt submission;
  fresh selections, unsaved edits, Unicode, multiple cursors and terminal focus.
- Multiple projects, nested roots, duplicate windows, root changes, closed editors
  and empty context in real Zed windows.
- Zed-first, official-app-first, simultaneous startup (including a stale Unix
  endpoint), and router-owner exit with the complete official apps.
- Native Windows pipe DACL/ownership and Linux peer credentials under different
  users; Windows and Linux runtime execution of the tests.
- Live Claude connection/selection notifications and Keep/Reject diff review.

No updated Zed application was launched or published during this verification.

## Follow-up audit

The double-check found a real gap in the first implementation: replacing a Unix
socket pathname did not close its existing streams, so Zed could remain registered
with an unreachable router. A regression test reproduced this failure before the
fix. Endpoint identity monitoring now forces rediscovery for both Zed-owned and
external routers. Separate tests cover each case and verify the replacement
endpoint still serves requests. This closes that recovery gap; it does not replace
the outstanding tests with complete native applications listed above.
