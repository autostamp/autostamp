#!/usr/bin/env python3
"""Offline component integration: python3 crates/autostamp-mcp/tests/component_runtime.py.

Requires cargo's wasm32-wasip2 target, wasm-tools, wac, and wasmtime. Builds
standalone generated clients and a test-only secrets provider, then invokes real
WASI HTTP against an ephemeral loopback server. No service credentials are used.
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
BASE = (
    '{mode: green, tiny: -128, double: some(1.5), empty: some(empty), nullable: none, map: some([("a", 32767)]), '
    'mixed: some({fixed: some("value"), additional-properties: [("extra", true)]}), '
    'floats: some([1.5]), unsigned: some(18446744073709551615), '
    'array: some([none, some("text")])}'
)
EXPECTED = {
    "mode": "green",
    "tiny": -128,
    "double": 1.5,
    "empty": {},
    "map": {"a": 32767},
    "mixed": {"fixed": "value", "extra": True},
    "floats": [1.5],
    "unsigned": 18446744073709551615,
    "array": [None, "text"],
}


def command(*args, **kwargs):
    result = subprocess.run(
        [str(arg) for arg in args],
        cwd=ROOT,
        text=True,
        stdout=subprocess.PIPE,
        stderr=subprocess.PIPE,
        timeout=240,
        **kwargs,
    )
    if result.returncode:
        raise AssertionError(
            f"command failed: {' '.join(map(str, args))}\n{result.stdout}\n{result.stderr}"
        )
    return result.stdout


class State:
    def __init__(self):
        self.scenario = "json"
        self.auth = False
        self.calls = []
        self.requests = []
        self.errors = []
        self.release_stream = threading.Event()
        self.stream_open = threading.Event()

    def reset(self, scenario, auth):
        self.release_stream.set()
        self.scenario = scenario
        self.auth = auth
        self.calls = []
        self.requests = []
        self.errors = []
        self.release_stream = threading.Event()
        self.stream_open = threading.Event()


STATE = State()


class Handler(BaseHTTPRequestHandler):
    protocol_version = "HTTP/1.1"

    def log_message(self, *_):
        pass

    def handle(self):
        try:
            super().handle()
        except (BrokenPipeError, ConnectionResetError):
            # The guest drops streams immediately after matching replies.
            pass

    def require(self, condition, message):
        if not condition:
            STATE.errors.append(message)

    def reply(self, status, data=b"", content_type="application/json", headers=()):
        self.send_response(status)
        self.send_header("Content-Type", content_type)
        self.send_header("Content-Length", str(len(data)))
        for name, value in headers:
            self.send_header(name, value)
        self.end_headers()
        self.wfile.write(data)

    def check_headers(self, initialized):
        self.require(
            self.headers.get("Accept") == "application/json, text/event-stream",
            "missing dual Accept",
        )
        self.require(
            self.headers.get("Authorization")
            == ("Bearer fixture-token" if STATE.auth else None),
            "unexpected Authorization",
        )
        self.require(
            self.headers.get("MCP-Session-Id")
            == ("session-fixture" if initialized else None),
            "session header was not forwarded",
        )
        self.require(
            self.headers.get("MCP-Protocol-Version")
            == ("2025-11-25" if initialized and STATE.scenario != "version" else None),
            "protocol header was not forwarded",
        )

    def do_DELETE(self):
        STATE.requests.append("DELETE")
        self.check_headers(True)
        self.reply(405 if STATE.scenario != "delete-error" else 500)

    def do_POST(self):
        try:
            self.handle_post()
        except (BrokenPipeError, ConnectionResetError):
            # Cancellation is expected for early SSE completion and timeout.
            pass

    def handle_post(self):
        if self.headers.get("Transfer-Encoding") == "chunked":
            chunks = []
            while True:
                size = int(self.rfile.readline().split(b";", 1)[0], 16)
                if size == 0:
                    while self.rfile.readline() != b"\r\n":
                        pass
                    break
                chunks.append(self.rfile.read(size))
                self.require(self.rfile.read(2) == b"\r\n", "invalid chunk delimiter")
            body = b"".join(chunks)
        else:
            body = self.rfile.read(int(self.headers["Content-Length"]))
        message = json.loads(body)
        method = message.get("method", "server-response")
        STATE.requests.append(method)
        self.check_headers(method != "initialize")
        scenario = STATE.scenario
        if method == "initialize":
            self.require(message["params"]["capabilities"] == {}, "unimplemented capabilities")
            if scenario in ("http-error", "redirect"):
                self.reply(
                    401 if scenario == "http-error" else 307,
                    b"do not expose fixture-token",
                    headers=(("Location", "/must-not-follow"),),
                )
                return
            result = {
                "protocolVersion": "future" if scenario == "version" else "2025-11-25",
                "capabilities": {} if scenario == "capability" else {"tools": {}},
                "serverInfo": {"name": "fixture", "version": "1"},
            }
            session = "bad session" if scenario == "session-id" else "session-fixture"
            data = json.dumps({"jsonrpc": "2.0", "id": message["id"], "result": result}).encode()
            if scenario == "rpc-id-decimal":
                data = data.replace(b'"id": 1', b'"id": 1.0')
            elif scenario == "rpc-id-exponent":
                data = data.replace(b'"id": 1', b'"id": 1e0')
            self.reply(200, data, headers=(("MCP-Session-Id", session),))
            return
        if method in ("notifications/initialized", "server-response"):
            if method == "server-response":
                if message["id"] == "server-ping":
                    self.require(message.get("result") == {}, "ping reply")
                else:
                    self.require(message.get("error", {}).get("code") == -32601, "unsupported reply")
            self.reply(
                200 if scenario == "ack-status" else 202,
                b"invalid" if scenario == "ack-body" else b"",
            )
            return
        self.require(method == "tools/call", "unexpected client method")
        STATE.calls.append(message["params"])
        if scenario == "expired":
            self.reply(404)
            return
        if scenario == "interrupted":
            self.send_response(200)
            self.send_header("Content-Type", "application/json")
            self.send_header("Content-Length", "100")
            self.end_headers()
            self.wfile.write(b"{")
            self.wfile.flush()
            self.close_connection = True
            return
        structured = copy.deepcopy(message["params"]["arguments"])
        mutations = {
            "missing-required": lambda: structured.pop("tiny"),
            "wrong-type": lambda: structured.update(tiny="not a number"),
            "integer-overflow": lambda: structured.update(tiny=128),
            "integer-underflow": lambda: structured.update(tiny=-129),
            "fractional-integer": lambda: structured.update(tiny=1.5),
            "enum": lambda: structured.update(mode="blue"),
            "closed-object": lambda: structured.update(unknown=True),
            "nonnull": lambda: structured.update(tiny=None),
            "map-type": lambda: structured.update(map={"key": False}),
            "map-range": lambda: structured.update(map={"key": 32768}),
            "mixed-type": lambda: structured.update(mixed={"extra": 7}),
            "float-range": lambda: structured.update(floats=[3.5e38]),
            "u64-range": lambda: structured.update(unsigned=18446744073709551616),
            "u64-negative": lambda: structured.update(unsigned=-1),
        }
        if scenario in mutations:
            mutations[scenario]()
        result = {
            "content": [{"type": "text", "text": "kept content"}],
            "structuredContent": structured,
            "_meta": {
                "trace": "kept metadata",
                "large": "RAW_LARGE",
                "tiny": "RAW_TINY",
                "literal": {"$serde_json::private::Number": "7"},
            },
        }
        if scenario in ("depth-maximum", "depth-overflow"):
            nested = 0
            for _ in range(125 if scenario == "depth-maximum" else 126):
                nested = [nested]
            result["_meta"]["nested"] = nested
        if scenario == "tool-error":
            result["isError"] = True
            result["structuredContent"] = {"invalid": "success schema must not be decoded"}
        elif scenario == "missing-structured":
            result.pop("structuredContent")
        elif scenario == "envelope-invalid":
            result["isError"] = "false"
        rpc = {"jsonrpc": "2.0", "id": message["id"], "result": result}
        if scenario == "rpc-error":
            rpc.pop("result")
            rpc["error"] = {"code": -32602, "message": "fixture-token session-fixture", "data": {"field": "name"}}
        if scenario == "wrong-id":
            rpc["id"] = "wrong"
        data = json.dumps(rpc, ensure_ascii=False).encode()
        data = data.replace(b'"RAW_LARGE"', b"18446744073709551617").replace(b'"RAW_TINY"', b"1e-999")
        numeric_spellings = {
            "integral-decimal": (b'"tiny": -128', b'"tiny": -128.0'),
            "integral-exponent": (b'"tiny": -128', b'"tiny": 1e2'),
            "integral-negative-zero": (b'"tiny": -128', b'"tiny": -0'),
            "integral-u64": (b'"unsigned": 18446744073709551615', b'"unsigned": 18446744073709551615.0'),
            "integral-u64-exponent": (b'"unsigned": 18446744073709551615', b'"unsigned": 18446744073709551615e0'),
            "integer-rounded-fraction": (b'"tiny": -128', b'"tiny": -127.9999999999999999999999999999'),
            "u64-rounded-fraction": (b'"unsigned": 18446744073709551615', b'"unsigned": 18446744073709551615.1'),
            "integer-exponent-overflow": (b'"tiny": -128', b'"tiny": 1.28e2'),
            "float64-underflow": (b'"double": 1.5', b'"double": 1e-999'),
            "float64-overflow": (b'"double": 1.5', b'"double": 1e999'),
            "float32-underflow": (b'"floats": [1.5]', b'"floats": [1e-999]'),
            "float32-cast-underflow": (b'"floats": [1.5]', b'"floats": [1e-90]'),
            "zero-exponent": (b'"double": 1.5', b'"double": 0e-999'),
            "rpc-id-decimal": (b'"id": 2', b'"id": 2.0'),
            "rpc-id-exponent": (b'"id": 2', b'"id": 2e0'),
            "rpc-id-fractional": (b'"id": 2', b'"id": 2.5'),
            "rpc-id-string": (b'"id": 2', b'"id": "2"'),
        }
        if scenario in numeric_spellings:
            old, new = numeric_spellings[scenario]
            assert old in data, (scenario, data)
            data = data.replace(old, new)
        if scenario == "duplicate-wire-key":
            data = data.replace(b'"tiny": -128', b'"tiny": 1, "tiny": 2')
        if scenario == "body-limit":
            self.reply(200, b" " * 4097)
            return
        if scenario in ("sse", "timeout", "event-limit", "truncated", "sse-body-limit"):
            self.sse(data, scenario)
        else:
            self.reply(200, data)

    def sse(self, data, scenario):
        self.send_response(200)
        self.send_header("Content-Type", "text/event-stream; charset=utf-8")
        self.send_header("Transfer-Encoding", "chunked")
        self.end_headers()

        def chunk(value):
            self.wfile.write(f"{len(value):x}\r\n".encode() + value + b"\r\n")
            self.wfile.flush()

        if scenario == "timeout":
            # Regular bytes prevent an idle timeout from masquerading as a total deadline.
            for _ in range(40):
                chunk(b": still active\n\n")
                time.sleep(0.05)
            return
        if scenario == "event-limit":
            chunk(b"data: " + b"x" * 1500)
        elif scenario == "sse-body-limit":
            chunk(b": comment\n\n" * 400)
        elif scenario == "truncated":
            chunk(b"data: " + data + b"\n")
        else:
            prefix = (
                b"\xef\xbb\xbf: comment\r\n\r\nid: prime\r\ndata:\r\n\r\n"
                b'data: {"jsonrpc":"2.0","id":"server-ping","method":"ping"}\r\n\r\n'
                b'data: {"jsonrpc":"2.0","id":"unsupported","method":"sampling/createMessage"}\r\n\r\n'
                + 'data: {"jsonrpc":"2.0","method":"notifications/progress","params":{"message":"caf\u00e9"}}\r\n\r\n'.encode()
            )
            payload = prefix + b"data: " + data + b"\r\n\r\ndata: NEVER READ"
            release = STATE.release_stream
            STATE.stream_open.set()
            for index in range(0, len(payload), 7):
                chunk(payload[index:index + 7])
            # The client must finish without EOF, and without consuming trailing invalid data.
            release.wait(5)
        self.wfile.write(b"0\r\n\r\n")
        self.wfile.flush()


def generate(destination, endpoint, authenticated):
    command(
        "cargo", "run", "--quiet", "-p", "autostamp-mcp", "--example", "generate", "--",
        CRATE / "tests/fixtures/runtime.json", f"fixture:runtime-{destination.name}@0.1.0",
        endpoint, destination, "mcp-test-token" if authenticated else "-",
        "400", "2048", "1024",
    )


def build_component(destination, cache):
    command(
        "cargo", "build", "--quiet", "--manifest-path", destination / "Cargo.toml",
        "--target", "wasm32-wasip2", "--target-dir", cache,
    )
    artifact = cache / f"wasm32-wasip2/debug/runtime_{destination.name}.wasm"
    command("wasm-tools", "validate", artifact)
    wit = command("wasm-tools", "component", "wit", artifact)
    assert "typed: func" in wit and "envelope: func" in wit, wit
    binary = destination / "client.wasm"
    shutil.copy2(artifact, binary)
    return binary, wit


def invoke(binary, scenario, function=None, expected="ok(", auth=False, calls=1):
    STATE.reset(scenario, auth)
    try:
        output = command(
            WASMTIME, "run", "-S", "http", "--allow-precompiled",
            "--invoke", function or f"typed({BASE})", binary,
        ).strip()
        assert output.startswith(expected), (scenario, output)
        assert not STATE.errors, (scenario, STATE.errors)
        assert len(STATE.calls) == calls, (scenario, STATE.calls)
        if scenario == "sse":
            assert STATE.stream_open.wait(0.2), "mock did not hold SSE open"
            assert not STATE.release_stream.is_set(), "SSE completed only after EOF"
            assert STATE.requests.count("server-response") == 2, STATE.requests
        if scenario == "expired":
            assert STATE.requests == ["initialize", "notifications/initialized", "tools/call"]
        elif calls == 1:
            assert STATE.requests.count("tools/call") == 1
            assert STATE.requests.count("DELETE") == 1, STATE.requests
        print(f"PASS {scenario}{' (bearer)' if auth else ''}")
        return output
    finally:
        STATE.release_stream.set()


def main():
    server = ThreadingHTTPServer(("127.0.0.1", 0), Handler)
    server.daemon_threads = True
    server_thread = threading.Thread(target=server.serve_forever, daemon=True)
    server_thread.start()
    endpoint = f"http://127.0.0.1:{server.server_port}/mcp"
    cache = ROOT / "target/mcp-component-tests"
    cache.mkdir(parents=True, exist_ok=True)
    try:
        with tempfile.TemporaryDirectory(prefix="fixture-", dir=cache) as temporary:
            base = Path(temporary)
            anonymous = base / "anonymous"
            authenticated = base / "authenticated"
            generate(anonymous, endpoint, False)
            binary, wit = build_component(anonymous, cache)
            assert "wasmcloud:secrets" not in wit, "anonymous client imports secret provider"
            command(WASMTIME, "compile", binary, "-o", base / "anonymous.cwasm")
            anonymous_binary = base / "anonymous.cwasm"

            generate(authenticated, endpoint, True)
            auth_binary, wit = build_component(authenticated, cache)
            assert "wasmcloud:secrets" in wit
            command(
                "cargo", "build", "--quiet", "--manifest-path", CRATE / "tests/secrets/Cargo.toml",
                "--target", "wasm32-wasip2", "--target-dir", cache,
            )
            command(
                "wac", "plug", "--plug", cache / "wasm32-wasip2/debug/mcp_test_secrets.wasm",
                auth_binary, "-o", base / "composed.wasm",
            )
            command("wasm-tools", "validate", base / "composed.wasm")
            command(WASMTIME, "compile", base / "composed.wasm", "-o", base / "auth.cwasm")

            output = invoke(anonymous_binary, "json")
            assert STATE.calls[0]["arguments"] == EXPECTED, STATE.calls
            assert "structured-content:" in output and "kept metadata" in output
            assert "18446744073709551617" in output and "1e-999" in output
            assert "$serde_json::private::Number" in output
            output = invoke(anonymous_binary, "sse")
            assert "18446744073709551617" in output and "1e-999" in output
            invoke(base / "auth.cwasm", "json", auth=True)
            invoke(base / "auth.cwasm", "sse", auth=True)
            output = invoke(anonymous_binary, "json", "envelope()")
            assert "kept content" in output and "kept metadata" in output
            assert "18446744073709551617" in output and "1e-999" in output
            for scenario, typed in [
                ("integral-decimal", "tiny: -128"),
                ("integral-exponent", "tiny: 100"),
                ("integral-negative-zero", "tiny: 0"),
                ("integral-u64", "unsigned: some(18446744073709551615)"),
                ("integral-u64-exponent", "unsigned: some(18446744073709551615)"),
                ("zero-exponent", "double: some(0)"),
                ("rpc-id-decimal", "tiny: -128"),
                ("rpc-id-exponent", "tiny: -128"),
                ("depth-maximum", "tiny: -128"),
            ]:
                output = invoke(anonymous_binary, scenario)
                assert typed in output, (scenario, output)
            for wave, value in [("none", "missing"), ("some(none)", None), ("some(some(yes))", "yes")]:
                output = invoke(anonymous_binary, "json", f"typed({BASE.replace('nullable: none', 'nullable: ' + wave)})")
                arguments = STATE.calls[0]["arguments"]
                if value == "missing":
                    assert "nullable" not in arguments
                else:
                    assert arguments["nullable"] == value
                if wave == "none":
                    assert "nullable:" not in output, output  # WAVE omits absent record options.
                else:
                    assert f"nullable: {wave}" in output, output

            for scenario in [
                "missing-required", "wrong-type", "integer-overflow", "integer-underflow",
                "fractional-integer", "enum", "closed-object", "nonnull", "map-type",
                "map-range", "mixed-type", "float-range", "u64-range", "u64-negative",
                "missing-structured", "integer-rounded-fraction", "u64-rounded-fraction",
                "integer-exponent-overflow", "float64-underflow", "float64-overflow", "float32-underflow",
                "float32-cast-underflow",
            ]:
                invoke(anonymous_binary, scenario, expected="err(result-decoding(")
            for scenario in [
                "wrong-id", "expired", "truncated", "body-limit", "event-limit",
                "sse-body-limit", "duplicate-wire-key", "envelope-invalid",
                "rpc-id-fractional", "rpc-id-string",
                "depth-overflow",
            ]:
                invoke(anonymous_binary, scenario, expected="err(protocol(")
            output = invoke(base / "auth.cwasm", "rpc-error", expected="err(protocol(", auth=True)
            assert "fixture-token" not in output and "session-fixture" not in output
            assert "-32602" in output and "field" in output
            output = invoke(anonymous_binary, "tool-error", expected="err(tool-execution(")
            assert "kept metadata" in output
            assert "18446744073709551617" in output and "1e-999" in output
            for scenario in ["timeout", "interrupted", "delete-error"]:
                invoke(anonymous_binary, scenario, expected="err(transport(")
            for scenario in ["version", "capability", "session-id", "ack-status", "ack-body"]:
                invoke(anonymous_binary, scenario, expected="err(protocol(", calls=0)
            for scenario in ["http-error", "redirect"]:
                output = invoke(base / "auth.cwasm", scenario, expected="err(transport(", auth=True, calls=0)
                assert "fixture-token" not in output
                assert STATE.requests == ["initialize"], STATE.requests
            for arguments in [
                BASE.replace('[("a", 32767)]', '[("a", 1), ("a", 2)]'),
                BASE.replace('[("extra", true)]', '[("fixed", true)]'),
                BASE.replace('[("extra", true)]', '[("extra", true), ("extra", false)]'),
                BASE.replace("[1.5]", "[nan]"),
                BASE.replace("[1.5]", "[inf]"),
            ]:
                invoke(anonymous_binary, "json", f"typed({arguments})", expected="err(result-decoding(", calls=0)
                assert STATE.requests == [], "invalid input performed network I/O"
            print("All generated-component integration checks passed.")
    finally:
        server.shutdown()
        server.server_close()
        server_thread.join()


if __name__ == "__main__":
    main()
