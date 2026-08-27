#!/usr/bin/env python3
"""Protocol probe for the claude_code_ide integration.

Drives a running Zed the way the Claude Code CLI does — discovery through the
lock file, WebSocket upgrade, JSON-RPC/MCP handshake, one `tools/call` — and
asserts the four Windows fixes in c0ca03cb still hold after a rebase:

    1. the lock file advertises `runningInWindows`, or the CLI skips the IDE
    2. the handshake echoes `Sec-WebSocket-Protocol`, or the CLI drops the socket
    3. URIs are `file:///C:/dir` on Windows, not `file://C:\\dir`
    4. display names come from `Path::file_name`, not `rsplit('/')`

Usage:
    python3 script/claude-ide-probe.py            # discover a single running Zed
    python3 script/claude-ide-probe.py --port 1234
    python3 script/claude-ide-probe.py -v         # dump every frame

Exit status is 0 only if every check passes. Standard library only, so it runs
unchanged on macOS, Linux and Windows.
"""

from __future__ import annotations

import argparse
import base64
import hashlib
import json
import os
import secrets
import socket
import struct
import sys
from pathlib import Path

# RFC 6455 section 1.3.
WS_GUID = "258EAFA5-E914-47DA-95CA-C5AB0DC85B11"

AUTH_HEADER = "x-claude-code-ide-authorization"
SUBPROTOCOL = "mcp"
MCP_PROTOCOL_VERSION = "2024-11-05"

EXPECTED_TOOLS = {
    "getCurrentSelection",
    "getLatestSelection",
    "getWorkspaceFolders",
    "getOpenEditors",
    "getDiagnostics",
    "openFile",
    "saveDocument",
    "checkDocumentDirty",
    "openDiff",
    "closeAllDiffTabs",
}

IS_WINDOWS = os.name == "nt"


class ProbeFailure(Exception):
    """A check failed. The message is what gets reported to the user."""


# --- reporting ---------------------------------------------------------------

_checks: list[tuple[bool, str, str]] = []


def check(ok: bool, label: str, detail: str = "") -> bool:
    _checks.append((ok, label, detail))
    mark = "PASS" if ok else "FAIL"
    line = f"  [{mark}] {label}"
    if detail:
        line += f" — {detail}"
    print(line, flush=True)
    return ok


def require(ok: bool, label: str, detail: str = "") -> None:
    if not check(ok, label, detail):
        raise ProbeFailure(label)


# --- lock file discovery -----------------------------------------------------


def lock_dir() -> Path:
    """`$CLAUDE_CONFIG_DIR/ide` if set and non-empty, else `~/.claude/ide`.

    Mirrors `lockfile::lock_dir` so the probe looks where the IDE writes.
    """
    configured = os.environ.get("CLAUDE_CONFIG_DIR")
    if configured:
        return Path(configured) / "ide"
    return Path.home() / ".claude" / "ide"


def find_lockfile(port: int | None) -> tuple[int, dict]:
    directory = lock_dir()
    if not directory.is_dir():
        raise ProbeFailure(f"no lock directory at {directory} — is Zed running?")

    if port is not None:
        candidates = [directory / f"{port}.lock"]
    else:
        candidates = sorted(directory.glob("*.lock"))

    live: list[tuple[int, dict, Path]] = []
    for path in candidates:
        if not path.is_file():
            continue
        try:
            contents = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, json.JSONDecodeError) as error:
            print(f"  [warn] ignoring {path.name}: {error}", flush=True)
            continue
        # Other IDEs write here too; only probe our own.
        if contents.get("ideName") != "Zed":
            continue
        if not pid_alive(contents.get("pid")):
            print(f"  [warn] ignoring {path.name}: stale (pid gone)", flush=True)
            continue
        live.append((int(path.stem), contents, path))

    if not live:
        raise ProbeFailure(
            f"no live Zed lock file in {directory}. Start Zed, or pass --port."
        )
    if len(live) > 1:
        ports = ", ".join(str(entry[0]) for entry in live)
        raise ProbeFailure(
            f"several Zed windows are listening ({ports}); pass --port to pick one"
        )

    found_port, contents, path = live[0]
    print(f"Lock file: {path}", flush=True)
    return found_port, contents


def pid_alive(pid: object) -> bool:
    if not isinstance(pid, int):
        return False
    if IS_WINDOWS:
        # No os.kill(pid, 0) on Windows; OpenProcess via tasklist is heavy, so
        # trust the file and let the connection attempt be the real liveness test.
        return True
    try:
        os.kill(pid, 0)
    except ProcessLookupError:
        return False
    except PermissionError:
        return True  # exists, owned by someone else
    return True


def check_lockfile(contents: dict) -> None:
    print("\nLock file contents", flush=True)
    require(contents.get("ideName") == "Zed", "ideName is 'Zed'", repr(contents.get("ideName")))
    require(contents.get("transport") == "ws", "transport is 'ws'", repr(contents.get("transport")))
    require(isinstance(contents.get("authToken"), str) and contents["authToken"],
            "authToken present")
    require(isinstance(contents.get("pid"), int), "pid present", str(contents.get("pid")))

    raw_folders = contents.get("workspaceFolders")
    require(isinstance(raw_folders, list) and len(raw_folders) > 0,
            "workspaceFolders is a non-empty list", repr(raw_folders))
    folders: list = raw_folders if isinstance(raw_folders, list) else []
    require(all(isinstance(folder, str) for folder in folders),
            "workspaceFolders are plain paths, not URIs")
    require(not any(str(folder).startswith("file://") for folder in folders),
            "workspaceFolders are not URI-encoded", repr(folders))

    # Fix 1: without this field the CLI skips a Windows IDE while scanning.
    require("runningInWindows" in contents,
            "runningInWindows is present (fix 1)")
    require(contents["runningInWindows"] is IS_WINDOWS,
            f"runningInWindows == {IS_WINDOWS}", repr(contents["runningInWindows"]))


def check_permissions() -> None:
    if IS_WINDOWS:
        return  # no Unix mode bits to check
    print("\nLock file permissions", flush=True)
    directory = lock_dir()
    check(directory.stat().st_mode & 0o077 == 0, "lock dir is not group/world accessible",
          oct(directory.stat().st_mode & 0o777))


# --- minimal WebSocket client ------------------------------------------------


class WebSocket:
    """Just enough of RFC 6455 to speak MCP: text frames, ping/pong, close."""

    def __init__(self, sock: socket.socket, verbose: bool) -> None:
        self._sock = sock
        self._buffer = b""
        self._verbose = verbose

    @classmethod
    def connect(cls, port: int, token: str, verbose: bool,
                subprotocol: str | None = SUBPROTOCOL) -> tuple["WebSocket", dict[str, str]]:
        sock = socket.create_connection(("127.0.0.1", port), timeout=15)
        key = base64.b64encode(secrets.token_bytes(16)).decode("ascii")

        lines = [
            "GET / HTTP/1.1",
            f"Host: 127.0.0.1:{port}",
            "Upgrade: websocket",
            "Connection: Upgrade",
            f"Sec-WebSocket-Key: {key}",
            "Sec-WebSocket-Version: 13",
            f"{AUTH_HEADER}: {token}",
        ]
        if subprotocol:
            lines.append(f"Sec-WebSocket-Protocol: {subprotocol}")
        request = ("\r\n".join(lines) + "\r\n\r\n").encode("ascii")
        sock.sendall(request)

        raw = b""
        while b"\r\n\r\n" not in raw:
            chunk = sock.recv(4096)
            if not chunk:
                raise ProbeFailure("server closed the connection during the handshake")
            raw += chunk
        head, _, rest = raw.partition(b"\r\n\r\n")
        text = head.decode("latin-1")
        status_line, *header_lines = text.split("\r\n")

        headers = {}
        for line in header_lines:
            name, _, value = line.partition(":")
            headers[name.strip().lower()] = value.strip()
        headers["__status__"] = status_line
        headers["__expected_accept__"] = base64.b64encode(
            hashlib.sha1((key + WS_GUID).encode("ascii")).digest()
        ).decode("ascii")

        websocket = cls(sock, verbose)
        websocket._buffer = rest
        return websocket, headers

    def _read_exactly(self, count: int) -> bytes:
        while len(self._buffer) < count:
            chunk = self._sock.recv(65536)
            if not chunk:
                raise ProbeFailure("connection closed while reading a frame")
            self._buffer += chunk
        data, self._buffer = self._buffer[:count], self._buffer[count:]
        return data

    def send_text(self, payload: str) -> None:
        if self._verbose:
            print(f"    -> {payload}", flush=True)
        data = payload.encode("utf-8")
        header = bytearray([0x81])  # FIN + text opcode
        length = len(data)
        # Client frames must be masked (RFC 6455 section 5.3).
        if length < 126:
            header.append(0x80 | length)
        elif length < (1 << 16):
            header.append(0x80 | 126)
            header += struct.pack("!H", length)
        else:
            header.append(0x80 | 127)
            header += struct.pack("!Q", length)
        mask = secrets.token_bytes(4)
        header += mask
        masked = bytes(byte ^ mask[index % 4] for index, byte in enumerate(data))
        self._sock.sendall(bytes(header) + masked)

    def recv_text(self) -> str:
        """Returns the next text frame, answering pings along the way."""
        while True:
            first, second = self._read_exactly(2)
            opcode = first & 0x0F
            masked = bool(second & 0x80)
            length = second & 0x7F
            if length == 126:
                (length,) = struct.unpack("!H", self._read_exactly(2))
            elif length == 127:
                (length,) = struct.unpack("!Q", self._read_exactly(8))
            mask = self._read_exactly(4) if masked else b""
            payload = self._read_exactly(length)
            if masked:
                payload = bytes(b ^ mask[i % 4] for i, b in enumerate(payload))

            if opcode == 0x1:  # text
                text = payload.decode("utf-8")
                if self._verbose:
                    print(f"    <- {text}", flush=True)
                return text
            if opcode == 0x9:  # ping -> pong
                self._sock.sendall(bytes([0x8A, 0x80]) + secrets.token_bytes(4))
                continue
            if opcode == 0x8:  # close
                raise ProbeFailure("server closed the WebSocket")
            # Ignore pong and continuation frames; MCP messages fit one frame.

    def close(self) -> None:
        try:
            self._sock.sendall(bytes([0x88, 0x80]) + secrets.token_bytes(4))
        except OSError:
            pass
        self._sock.close()


# --- JSON-RPC ----------------------------------------------------------------


class Rpc:
    def __init__(self, websocket: WebSocket) -> None:
        self._websocket = websocket
        self._next_id = 0

    def request(self, method: str, params: dict | None = None) -> dict:
        self._next_id += 1
        request_id = self._next_id
        message: dict[str, object] = {"jsonrpc": "2.0", "id": request_id, "method": method}
        if params is not None:
            message["params"] = params
        self._websocket.send_text(json.dumps(message))

        # Skip any server-initiated notification that arrives first.
        while True:
            response = json.loads(self._websocket.recv_text())
            if response.get("id") == request_id:
                break
        if "error" in response:
            raise ProbeFailure(f"{method} returned an error: {response['error']}")
        return response.get("result", {})

    def notify(self, method: str, params: dict | None = None) -> None:
        message: dict[str, object] = {"jsonrpc": "2.0", "method": method}
        if params is not None:
            message["params"] = params
        self._websocket.send_text(json.dumps(message))


def tool_payload(result: dict) -> dict:
    """Unwraps `{"content": [{"type": "text", "text": "<json>"}]}`."""
    content = result.get("content")
    if not isinstance(content, list) or not content:
        raise ProbeFailure(f"tool result has no content: {result!r}")
    text = content[0].get("text")
    if not isinstance(text, str):
        raise ProbeFailure(f"tool content is not text: {content[0]!r}")
    try:
        return json.loads(text)
    except json.JSONDecodeError as error:
        raise ProbeFailure(f"tool content is not JSON: {error}") from error


# --- checks ------------------------------------------------------------------


def check_rejects_bad_token(port: int, verbose: bool) -> None:
    print("\nAuthorization", flush=True)
    try:
        _, headers = WebSocket.connect(port, "not-the-right-token", verbose)
    except ProbeFailure:
        # A reset instead of a 401 still means the token was refused.
        check(True, "a wrong authToken is refused", "connection closed")
        return
    status = headers["__status__"]
    check("401" in status, "a wrong authToken is refused with 401", status)


def check_handshake(headers: dict[str, str]) -> None:
    print("\nWebSocket handshake", flush=True)
    status = headers["__status__"]
    require("101" in status, "server returns 101 Switching Protocols", status)
    require(headers.get("sec-websocket-accept") == headers["__expected_accept__"],
            "Sec-WebSocket-Accept is correct")
    # Fix 2: RFC 6455 requires the server to name its choice; a client whose
    # offer goes unanswered must fail the connection, which is how this
    # surfaced as "Connection reset without closing handshake" after the 101.
    require(headers.get("sec-websocket-protocol") == SUBPROTOCOL,
            f"server echoes Sec-WebSocket-Protocol: {SUBPROTOCOL} (fix 2)",
            repr(headers.get("sec-websocket-protocol")))


def check_initialize(rpc: Rpc) -> None:
    print("\nMCP handshake", flush=True)
    result = rpc.request("initialize", {
        "protocolVersion": MCP_PROTOCOL_VERSION,
        "capabilities": {},
        "clientInfo": {"name": "claude-ide-probe", "version": "1"},
    })
    require(result.get("protocolVersion") == MCP_PROTOCOL_VERSION,
            f"protocolVersion is {MCP_PROTOCOL_VERSION}", repr(result.get("protocolVersion")))
    server = result.get("serverInfo", {})
    require(server.get("name") == "zed", "serverInfo.name is 'zed'", repr(server.get("name")))
    check(bool(server.get("version")), "serverInfo.version present", repr(server.get("version")))
    rpc.notify("notifications/initialized")


def check_tools_list(rpc: Rpc) -> None:
    print("\ntools/list", flush=True)
    tools = rpc.request("tools/list").get("tools", [])
    names = {tool.get("name") for tool in tools}
    missing = EXPECTED_TOOLS - names
    require(not missing, "every expected tool is advertised",
            f"missing {sorted(missing)}" if missing else f"{len(names)} tools")
    check(all(isinstance(tool.get("inputSchema"), dict) for tool in tools),
          "every tool carries an inputSchema")


def check_workspace_folders(rpc: Rpc, expected_paths: list[str]) -> None:
    print("\ngetWorkspaceFolders (fixes 3 and 4)", flush=True)
    payload = tool_payload(rpc.request(
        "tools/call", {"name": "getWorkspaceFolders", "arguments": {}}
    ))
    require(payload.get("success") is True, "call succeeded", repr(payload.get("success")))

    raw_folders = payload.get("folders")
    require(bool(isinstance(raw_folders, list) and raw_folders),
            "folders is non-empty", repr(raw_folders))
    folder = raw_folders[0] if isinstance(raw_folders, list) else {}
    path, name, uri = folder.get("path", ""), folder.get("name", ""), folder.get("uri", "")
    print(f"    path={path!r} name={name!r} uri={uri!r}", flush=True)

    # Fix 4: `rsplit('/')` never splits a Windows path, so `name` came back as
    # the whole path. `Path.name` here is the platform's own answer.
    expected_name = Path(path).name
    require(name == expected_name,
            "name is the last path component, not the whole path (fix 4)",
            f"expected {expected_name!r}")
    # Backslash is checked explicitly, not just via os.sep, so a Windows-style
    # name leaking through is still caught when the probe runs on POSIX.
    require(os.sep not in name and "/" not in name and "\\" not in name,
            "name contains no separator (fix 4)", repr(name))

    # Fix 3: `file://C:\dir` parses `C:` as the URI authority. The three-slash form
    # is what every file URI needs on Windows.
    require(uri.startswith("file:///"),
            "uri uses the file:/// form (fix 3)", repr(uri))
    require("\\" not in uri, "uri contains no backslash (fix 3)", repr(uri))
    if IS_WINDOWS:
        drive = Path(path).drive  # e.g. "C:"
        if drive:
            require(uri.lower().startswith(f"file:///{drive.lower()}/"),
                    f"uri is file:///{drive}/… (fix 3)", repr(uri))

    if expected_paths:
        require(path in expected_paths,
                "folder path matches the lock file", f"lock file has {expected_paths}")

    root = payload.get("rootPath")
    check(root == path, "rootPath is the first folder", repr(root))


def check_open_editors(rpc: Rpc) -> None:
    """Advisory: only meaningful when a file is open, so nothing here is fatal."""
    print("\ngetOpenEditors (advisory)", flush=True)
    payload = tool_payload(rpc.request(
        "tools/call", {"name": "getOpenEditors", "arguments": {}}
    ))
    editors = payload.get("tabs") or payload.get("editors") or []
    if not editors:
        print("    [skip] no editors open — open a file to exercise this", flush=True)
        return
    for editor in editors:
        uri = editor.get("uri", "")
        if uri:
            check(uri.startswith("file:///") and "\\" not in uri,
                  f"editor uri is well formed (fix 3)", repr(uri))
        label = editor.get("label") or editor.get("name") or ""
        if label:
            check(os.sep not in label and "/" not in label,
                  "editor label is a display name (fix 4)", repr(label))


# --- entry point -------------------------------------------------------------


def main() -> int:
    parser = argparse.ArgumentParser(description=__doc__,
                                     formatter_class=argparse.RawDescriptionHelpFormatter)
    parser.add_argument("--port", type=int, help="probe this port instead of discovering one")
    parser.add_argument("-v", "--verbose", action="store_true", help="dump every frame")
    arguments = parser.parse_args()

    print(f"claude_code_ide probe — platform={sys.platform} windows={IS_WINDOWS}\n", flush=True)

    websocket = None
    try:
        port, lock = find_lockfile(arguments.port)
        check_lockfile(lock)
        check_permissions()
        check_rejects_bad_token(port, arguments.verbose)

        websocket, headers = WebSocket.connect(port, lock["authToken"], arguments.verbose)
        check_handshake(headers)

        rpc = Rpc(websocket)
        check_initialize(rpc)
        check_tools_list(rpc)
        check_workspace_folders(rpc, lock.get("workspaceFolders", []))
        check_open_editors(rpc)
    except ProbeFailure as error:
        print(f"\nprobe aborted: {error}", file=sys.stderr, flush=True)
    except OSError as error:
        print(f"\nprobe aborted: {error}", file=sys.stderr, flush=True)
    finally:
        if websocket is not None:
            websocket.close()

    failed = [label for ok, label, _ in _checks if not ok]
    total = len(_checks)
    print(f"\n{total - len(failed)}/{total} checks passed", flush=True)
    if failed:
        for label in failed:
            print(f"  FAILED: {label}", flush=True)
        return 1
    if total == 0:
        return 1
    return 0


if __name__ == "__main__":
    sys.exit(main())
