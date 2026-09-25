#!/usr/bin/env python3
"""Mock-only native discovery -> standalone WASIp2 build -> explicit tool call.

Requires cargo, wasm32-wasip2, wasm-tools, wac, and wasmtime (or WASMTIME).
All protocol traffic uses ephemeral loopback sockets; no real credentials.
"""

import copy
import json
import os
from pathlib import Path
import shutil
import subprocess
import tempfile
import threading
import time
from http.server import BaseHTTPRequestHandler, ThreadingHTTPServer


ROOT = Path(__file__).resolve().parents[3]
CRATE = ROOT / "crates/autostamp-mcp"
WASMTIME = os.environ.get("WASMTIME", "wasmtime")
TOKEN = "fixture-token"
SESSION = "cli-fixture-session"
TOOLS = [
    {"name": name, "description": f"Call {name}.",
     "inputSchema": {"type": "object", "additionalProperties": False}}
    for name in ["ping", "pong"]
]


def command(*args, expected=0, env=None):
    result = subprocess.run(
        [str(arg) for arg in args], cwd=ROOT, env=env, text=True,
        stdout=subprocess.PIPE, stderr=subprocess.PIPE, timeout=240,
    )
    assert result.returncode == expected, (
        args, result.returncode, result.stdout, result.stderr
    )
    assert TOKEN not in result.stdout + result.stderr
    assert SESSION not in result.stdout + result.stderr
    return result


class State:
    def __init__(self):
        self.release = threading.Event()
        self.reset("json")

    def reset(self, scenario, auth=False):
        self.release.set()
        self.release = threading.Event()
        self.scenario = scenario
        self.auth = auth
        self.token = TOKEN
        self.requests = []
        self.calls = []
        self.errors = []
        self.streams = []


STATE = State()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def setup(self):
        super().setup()
        self.connection.settimeout(5)

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError, TimeoutError):
            pass

    def require(self, condition, message):
        if not condition:
            STATE.errors.append(message)

    def reply(self, status, data=b"", headers=()):
        self.send_response(status)
        self.send_header("Content-Type", "application/json")
        self.send_header("Content-Length", str(len(data)))
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)

    def rpc(self, message, result, initialized=False):
        data = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
        headers = [("MCP-Session-Id", SESSION)] if initialized else []
        if STATE.scenario != "sse":
            self.reply(200, data, headers)
            return
        release = STATE.release
        finished = threading.Event()
        STATE.streams.append(finished)
        try:
            self.send_response(200)
            self.send_header("Content-Type", "text/event-stream")
            self.send_header("Transfer-Encoding", "chunked")
            for name, value in headers:
                self.send_header(name, value)
            self.end_headers()
            event = b": comment\r\nevent: message\r\ndata: " + data + b"\r\n\r\n"
            for start in range(0, len(event), 7):
                chunk = event[start:start + 7]
                self.wfile.write(f"{len(chunk):x}\r\n".encode() + chunk + b"\r\n")
            self.wfile.flush()
            release.wait(5)
            self.wfile.write(b"0\r\n\r\n")
            self.wfile.flush()
        finally:
            finished.set()

    def check_headers(self, initialized, closing=False):
        self.require(self.headers.get("Authorization") == (
            f"Bearer {STATE.token}" if STATE.auth else None
        ), "unexpected authorization")
        self.require(self.headers.get("MCP-Session-Id") == (
            SESSION if initialized else None
        ), "unexpected session")
        version = self.headers.get("MCP-Protocol-Version")
        if closing:
            self.require(version in (None, "2025-11-25"), "unexpected close version")
        else:
            self.require(version == ("2025-11-25" if initialized else None), "unexpected version")
        self.require(self.headers.get("Accept") == "application/json, text/event-stream", "accept")

    def do_DELETE(self):
        STATE.requests.append("DELETE")
        self.check_headers(True, closing=True)
        self.reply(500 if STATE.scenario == "cleanup" else 204)

    def do_POST(self):
        if self.headers.get("Transfer-Encoding") == "chunked":
            chunks = []
            while True:
                size = int(self.rfile.readline().split(b";", 1)[0], 16)
                if not size:
                    self.require(self.rfile.readline() == b"\r\n", "chunk trailer")
                    break
                chunks.append(self.rfile.read(size))
                self.require(self.rfile.read(2) == b"\r\n", "chunk delimiter")
            body = b"".join(chunks)
        else:
            body = self.rfile.read(int(self.headers["Content-Length"]))
        message = json.loads(body)
        method = message["method"]
        STATE.requests.append(method)
        self.check_headers(method != "initialize")
        scenario = STATE.scenario
        if method == "initialize":
            self.require(message["params"]["capabilities"] == {}, "capabilities")
            if scenario == "auth":
                self.reply(401, f"Never print {TOKEN} {SESSION}".encode())
                return
            if scenario == "timeout":
                time.sleep(0.2)
            self.rpc(message, {
                "protocolVersion": "future" if scenario == "version" else "2025-11-25",
                "capabilities": {} if scenario == "capability" else {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "1"},
            }, initialized=True)
        elif method == "notifications/initialized":
            self.reply(202)
        elif method == "tools/list":
            cursor = message["params"].get("cursor")
            self.require(cursor in (None, ""), "opaque empty cursor was not forwarded")
            if scenario == "malformed":
                self.reply(200, b"invalid")
                return
            if scenario == "rpc":
                self.reply(200, json.dumps({
                    "jsonrpc": "2.0", "id": message["id"],
                    "error": {"code": -32602, "message": f"{TOKEN} {SESSION} field failure"},
                }).encode())
                return
            page = {"tools": [TOOLS[1]], "nextCursor": ""} if cursor is None else {"tools": [TOOLS[0]]}
            page = copy.deepcopy(page)
            if scenario == "duplicate" and cursor is not None:
                page["tools"] = [TOOLS[1]]
            elif scenario == "cursor":
                page = {"tools": [], "nextCursor": ""}
            elif scenario == "null-cursor":
                page["nextCursor"] = None
            elif scenario == "bad-page":
                page = {"tools": None}
            elif scenario == "empty":
                page = {"tools": []}
            elif scenario == "schema":
                page["tools"][0]["inputSchema"] = {"type": "object"}
            elif scenario == "echo":
                page["tools"][0]["description"] = STATE.token
            self.rpc(message, page)
        elif method == "tools/call":
            STATE.calls.append(message["params"])
            self.require(message["params"] == {"name": "ping", "arguments": {}}, "call arguments")
            self.rpc(message, {"content": [{"type": "text", "text": "explicit invocation only"}]})
        else:
            STATE.errors.append(f"unexpected method {method}")
            self.reply(400)


def release_streams():
    STATE.release.set()
    for finished in STATE.streams:
        assert finished.wait(6), "fixture streaming handler did not finish"


def main():
    command("cargo", "build", "--quiet", "-p", "autostamp-mcp", "--features", "cli", "--bin", "autostamp-mcp")
    metadata = json.loads(command("cargo", "metadata", "--format-version", "1", "--no-deps").stdout)
    executable = Path(metadata["target_directory"]) / "debug" / (
        "autostamp-mcp.exe" if os.name == "nt" else "autostamp-mcp"
    )
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    thread = threading.Thread(target=server.serve_forever, daemon=True)
    thread.start()
    endpoint = f"http://127.0.0.1:{server.server_port}/mcp"
    cache = ROOT / "target/mcp-cli-tests"
    cache.mkdir(parents=True, exist_ok=True)
    passed = 0
    try:
        with tempfile.TemporaryDirectory(prefix="fixture-", dir=cache) as temporary:
            base = Path(temporary).resolve()

            def cli(output, *args, expected=0, auth=False, offline=None):
                env = dict(os.environ)
                env.pop("AUTOSTAMP_CLI_TEST_TOKEN", None)
                inputs = ["--catalog", offline, "--endpoint", endpoint] if offline else ["--server", endpoint]
                if auth:
                    env["AUTOSTAMP_CLI_TEST_TOKEN"] = STATE.token
                    inputs += ["--token-env", "AUTOSTAMP_CLI_TEST_TOKEN"]
                result = command(
                    executable, *inputs, "--package", f"fixture:cli-{output.name.replace(' ', '-')}@0.1.0",
                    "--output", output, "--timeout-ms", "1000", *args,
                    expected=expected, env=env,
                )
                assert not STATE.calls, "generation invoked a tool"
                assert not STATE.errors, (STATE.scenario, STATE.errors)
                if expected == 0:
                    for path in output.rglob("*"):
                        if path.is_file():
                            content = path.read_bytes()
                            assert TOKEN.encode() not in content and SESSION.encode() not in content, path
                return result

            def build(output):
                command("cargo", "build", "--quiet", "--manifest-path", output / "Cargo.toml",
                        "--target", "wasm32-wasip2", "--target-dir", cache)
                source = cache / f"wasm32-wasip2/debug/cli_{output.name.replace(' ', '_')}.wasm"
                command("wasm-tools", "validate", source)
                wit = command("wasm-tools", "component", "wit", source).stdout
                assert "ping: func" in wit and "pong: func" in wit
                binary = output / "client.wasm"
                shutil.copyfile(source, binary)
                return binary

            output = base / "live crate"
            for scenario in ["json", "sse"]:
                STATE.reset(scenario)
                try:
                    cli(output)
                    assert STATE.requests == [
                        "initialize", "notifications/initialized", "tools/list", "tools/list", "DELETE",
                    ], STATE.requests
                    if scenario == "sse":
                        assert STATE.streams and not STATE.release.is_set()
                finally:
                    release_streams()
                passed += 1
            binary = build(output)
            for scenario in ["json", "sse"]:
                STATE.reset(scenario)
                try:
                    result = command(WASMTIME, "run", "-S", "http", "--invoke", "ping()", binary)
                    assert result.stdout.startswith("ok(") and "explicit invocation only" in result.stdout
                    assert len(STATE.calls) == 1 and not STATE.errors, STATE.errors
                    assert STATE.requests == ["initialize", "notifications/initialized", "tools/call", "DELETE"]
                finally:
                    release_streams()
                passed += 1

            STATE.reset("json", auth=True)
            authenticated = base / "auth"
            cli(authenticated, "--bearer-secret", "mcp-test-token", auth=True)
            auth_binary = build(authenticated)
            command("cargo", "build", "--quiet", "--manifest-path", CRATE / "tests/secrets/Cargo.toml",
                    "--target", "wasm32-wasip2", "--target-dir", cache)
            composed = base / "composed.wasm"
            command("wac", "plug", "--plug", cache / "wasm32-wasip2/debug/mcp_test_secrets.wasm",
                    auth_binary, "-o", composed)
            command("wasm-tools", "validate", composed)
            STATE.reset("json", auth=True)
            result = command(WASMTIME, "run", "-S", "http", "--invoke", "ping()", composed)
            assert result.stdout.startswith("ok(") and len(STATE.calls) == 1
            assert not STATE.errors, STATE.errors
            passed += 1

            STATE.reset("json")
            (authenticated / "user.txt").write_text("keep")
            cli(authenticated)
            assert not (authenticated / "wit/deps/wasmcloud-secrets").exists()
            assert (authenticated / "user.txt").read_text() == "keep"
            anonymous_again = build(authenticated)
            assert "wasmcloud:secrets" not in command(
                "wasm-tools", "component", "wit", anonymous_again,
            ).stdout
            passed += 1

            STATE.reset("json", auth=True)
            STATE.token = "serde_json"
            cli(base / "template-coincidence", auth=True)
            passed += 1

            STATE.reset("json")
            catalog = base / "catalog.json"
            catalog.write_text(json.dumps({"tools": TOOLS}))
            offline = base / "offline"
            cli(offline, offline=catalog)
            assert not STATE.requests
            assert (offline / "wit/world.wit").read_text().splitlines()[1:] == (
                output / "wit/world.wit"
            ).read_text().splitlines()[1:]
            passed += 1

            STATE.reset("empty")
            empty = base / "empty"
            cli(empty)
            assert not (empty / "wit/world.wit").read_text().count(": func(")
            command("cargo", "build", "--quiet", "--manifest-path", empty / "Cargo.toml",
                    "--target", "wasm32-wasip2", "--target-dir", cache)
            command("wasm-tools", "validate", cache / "wasm32-wasip2/debug/cli_empty.wasm")
            passed += 1

            failures = [
                ("capability", [], "tools capability"),
                ("version", [], "unsupported negotiated protocol"),
                ("auth", [], "HTTP status 401"),
                ("malformed", [], "protocol:"),
                ("bad-page", [], "tools array"),
                ("duplicate", [], "duplicate tool"),
                ("cursor", [], "repeated pagination"),
                ("null-cursor", [], "nextCursor must be a string"),
                ("schema", [], "/tools/"),
                ("rpc", [], "-32602"),
                ("cleanup", [], "HTTP status 500"),
                ("echo", [], "discovery credential"),
                ("json", ["--bearer-secret", TOKEN], "discovery credential"),
                ("json", ["--endpoint", endpoint + "/" + TOKEN], "discovery credential"),
                ("json", ["--max-pages", "1"], "page limit"),
                ("json", ["--max-tools", "1"], "tool limit"),
                ("json", ["--max-catalog-bytes", "12"], "catalog byte limit"),
                ("json", ["--max-response-bytes", "100", "--max-event-bytes", "100"], "byte limit"),
                ("sse", ["--max-event-bytes", "10"], "limit"),
            ]
            for index, (scenario, flags, expected) in enumerate(failures):
                STATE.reset(scenario, auth=True)
                destination = base / f"failure-case{index}"
                try:
                    result = cli(destination, *flags, expected=1, auth=True)
                    assert expected in result.stderr, (scenario, result.stderr)
                    assert not destination.exists(), "failed discovery wrote partial output"
                finally:
                    release_streams()
                passed += 1
            print(f"All {passed} CLI discovery/build/invoke integration checks passed.")
    finally:
        release_streams()
        server.shutdown()
        server.server_close()
        thread.join(timeout=6)
        assert not thread.is_alive(), "fixture server failed to stop"


if __name__ == "__main__":
    main()
