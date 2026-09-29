//! Offline integration of generated components with real WASI HTTP and secrets.
//!
//! Run with `cargo test -p autostamp-mcp --features cli --test component_runtime
//! -- --ignored`. Requires wasm32-wasip2, wasm-tools, wac, and wasmtime.

#![cfg(all(feature = "cli", not(target_arch = "wasm32")))]

mod common;

use std::collections::BTreeMap;
use std::ffi::OsStr;
use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use autostamp_mcp::{
    GenerateOptions, PackageName, artifact_writer, generate, parse_catalog, runtime::Limits,
};
use common::{PROTOCOL, Reply, Request, SESSION, Server, Stream, TOKEN, TempDir};
use serde_json::{Value, json};

const BASE: &str = concat!(
    "{mode: green, tiny: -128, double: some(1.5), empty: some(empty), nullable: none, ",
    "map: some([(\"a\", 32767)]), ",
    "mixed: some({fixed: some(\"value\"), additional-properties: [(\"extra\", true)]}), ",
    "floats: some([1.5]), unsigned: some(18446744073709551615), ",
    "array: some([none, some(\"text\")])}"
);

#[derive(Clone, Debug)]
struct State {
    scenario: String,
    authenticated: bool,
    calls: Vec<Value>,
    requests: Vec<String>,
    errors: Vec<String>,
    release: Arc<AtomicBool>,
    opened: Arc<AtomicBool>,
}

impl State {
    fn new(scenario: &str, authenticated: bool) -> Self {
        Self {
            scenario: scenario.into(),
            authenticated,
            calls: Vec::new(),
            requests: Vec::new(),
            errors: Vec::new(),
            release: Arc::new(AtomicBool::new(false)),
            opened: Arc::new(AtomicBool::new(false)),
        }
    }

    fn reset(&mut self, scenario: &str, authenticated: bool) {
        self.release.store(true, Ordering::SeqCst);
        *self = Self::new(scenario, authenticated);
    }

    fn require(&mut self, condition: bool, message: &str) {
        if !condition {
            self.errors.push(message.into());
        }
    }

    fn check_headers(&mut self, request: &Request, initialized: bool) {
        self.require(
            request.header("Accept") == Some("application/json, text/event-stream"),
            "missing dual Accept",
        );
        let authorization = self.authenticated.then(|| format!("Bearer {TOKEN}"));
        self.require(
            request.header("Authorization") == authorization.as_deref(),
            "unexpected Authorization",
        );
        self.require(
            request.header("MCP-Session-Id") == initialized.then_some(SESSION),
            "session header was not forwarded",
        );
        self.require(
            request.header("MCP-Protocol-Version")
                == (initialized && self.scenario != "version").then_some(PROTOCOL),
            "protocol header was not forwarded",
        );
    }

    fn handle(&mut self, request: Request) -> Reply {
        if request.method == "DELETE" {
            self.requests.push("DELETE".into());
            self.check_headers(&request, true);
            return Reply::empty(if self.scenario == "delete-error" {
                500
            } else {
                405
            });
        }
        self.require(request.method == "POST", "unexpected HTTP method");
        let message: Value = match serde_json::from_slice(&request.body) {
            Ok(message) => message,
            Err(error) => {
                self.errors.push(format!("invalid request JSON: {error}"));
                return Reply::empty(400);
            }
        };
        let method = message
            .get("method")
            .and_then(Value::as_str)
            .unwrap_or("server-response");
        self.requests.push(method.into());
        self.check_headers(&request, method != "initialize");
        match method {
            "initialize" => self.initialize(&message),
            "notifications/initialized" | "server-response" => {
                if method == "server-response" {
                    if message.get("id") == Some(&json!("server-ping")) {
                        self.require(message.get("result") == Some(&json!({})), "ping reply");
                    } else {
                        self.require(
                            message.pointer("/error/code") == Some(&json!(-32601)),
                            "unsupported reply",
                        );
                    }
                }
                Reply::json(
                    if self.scenario == "ack-status" {
                        200
                    } else {
                        202
                    },
                    if self.scenario == "ack-body" {
                        b"invalid".to_vec()
                    } else {
                        Vec::new()
                    },
                )
            }
            _ => {
                self.require(method == "tools/call", "unexpected client method");
                self.calls
                    .push(message.get("params").cloned().unwrap_or(Value::Null));
                self.tool_reply(&message)
            }
        }
    }

    fn initialize(&mut self, message: &Value) -> Reply {
        self.require(
            message.pointer("/params/capabilities") == Some(&json!({})),
            "unimplemented capabilities",
        );
        if matches!(self.scenario.as_str(), "http-error" | "redirect") {
            let mut reply = Reply::json(
                if self.scenario == "http-error" {
                    401
                } else {
                    307
                },
                format!("do not expose {TOKEN}").into_bytes(),
            );
            reply
                .headers
                .push(("Location".into(), "/must-not-follow".into()));
            return reply;
        }
        let mut data = json!({
            "jsonrpc": "2.0",
            "id": message.get("id"),
            "result": {
                "protocolVersion": if self.scenario == "version" { "future" } else { PROTOCOL },
                "capabilities": if self.scenario == "capability" { json!({}) } else { json!({"tools": {}}) },
                "serverInfo": {"name": "fixture", "version": "1"}
            }
        })
        .to_string();
        match self.scenario.as_str() {
            "rpc-id-decimal" => replace_wire(&mut data, r#""id":1"#, r#""id":1.0"#),
            "rpc-id-exponent" => replace_wire(&mut data, r#""id":1"#, r#""id":1e0"#),
            _ => {}
        }
        let mut reply = Reply::json(200, data.into_bytes());
        reply.headers.push((
            "MCP-Session-Id".into(),
            if self.scenario == "session-id" {
                "bad session"
            } else {
                SESSION
            }
            .into(),
        ));
        reply
    }

    fn tool_reply(&self, message: &Value) -> Reply {
        if self.scenario == "expired" {
            return Reply::empty(404);
        }
        if self.scenario == "interrupted" {
            let mut reply = Reply::json(200, b"{".to_vec());
            reply.headers.push(("Content-Length".into(), "100".into()));
            return reply;
        }
        if self.scenario == "body-limit" {
            return Reply::json(200, vec![b' '; 4097]);
        }
        let data = tool_response(message, &self.scenario);
        match self.scenario.as_str() {
            "sse" | "timeout" | "event-limit" | "truncated" | "sse-body-limit" => self.sse(data),
            _ => Reply::json(200, data.into_bytes()),
        }
    }

    fn sse(&self, data: String) -> Reply {
        let mut stream = Stream {
            gate_before_chunk: None,
            chunks: Vec::new(),
            interval: Duration::ZERO,
            release: None,
            opened: None,
            terminate: true,
        };
        match self.scenario.as_str() {
            "timeout" => {
                // Frequent bytes distinguish a total deadline from an idle timeout.
                stream.chunks = vec![b": still active\n\n".to_vec(); 40];
                stream.interval = Duration::from_millis(50);
                stream.terminate = false;
            }
            "event-limit" => {
                stream
                    .chunks
                    .push(format!("data: {}", "x".repeat(1500)).into_bytes());
            }
            "sse-body-limit" => stream.chunks.push(b": comment\n\n".repeat(400)),
            "truncated" => stream.chunks.push(format!("data: {data}\n").into_bytes()),
            _ => {
                let payload = format!(
                    concat!(
                        "\u{feff}: comment\r\n\r\nid: prime\r\ndata:\r\n\r\n",
                        "data: {{\"jsonrpc\":\"2.0\",\"id\":\"server-ping\",\"method\":\"ping\"}}\r\n\r\n",
                        "data: {{\"jsonrpc\":\"2.0\",\"id\":\"unsupported\",\"method\":\"sampling/createMessage\"}}\r\n\r\n",
                        "data: {{\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{{\"message\":\"café\"}}}}\r\n\r\n",
                        "data: {}\r\n\r\ndata: NEVER READ"
                    ),
                    data,
                );
                // Split UTF-8 and JSON across HTTP chunks; hold the stream past the reply.
                stream.chunks = payload.as_bytes().chunks(7).map(<[u8]>::to_vec).collect();
                stream.release = Some(Arc::clone(&self.release));
                stream.opened = Some(Arc::clone(&self.opened));
            }
        }
        let mut reply = Reply::json(200, Vec::new());
        reply
            .headers
            .retain(|(name, _)| !name.eq_ignore_ascii_case("Content-Type"));
        reply.headers.push((
            "Content-Type".into(),
            "text/event-stream; charset=utf-8".into(),
        ));
        reply.stream = Some(stream);
        reply
    }
}

fn replace_wire(data: &mut String, old: &str, new: &str) {
    assert!(data.contains(old), "missing raw spelling {old:?} in {data}");
    *data = data.replace(old, new);
}

fn tool_response(message: &Value, scenario: &str) -> String {
    let mut structured = message
        .pointer("/params/arguments")
        .and_then(Value::as_object)
        .expect("tools/call arguments must be an object")
        .clone();
    if scenario == "missing-required" {
        structured.remove("tiny");
    }
    let mutation = match scenario {
        "wrong-type" => Some(("tiny", json!("not a number"))),
        "integer-overflow" => Some(("tiny", json!(128))),
        "integer-underflow" => Some(("tiny", json!(-129))),
        "fractional-integer" => Some(("tiny", json!(1.5))),
        "enum" => Some(("mode", json!("blue"))),
        "closed-object" => Some(("unknown", json!(true))),
        "nonnull" => Some(("tiny", Value::Null)),
        "map-type" => Some(("map", json!({"key": false}))),
        "map-range" => Some(("map", json!({"key": 32768}))),
        "mixed-type" => Some(("mixed", json!({"extra": 7}))),
        "float-range" => Some(("floats", json!([3.5e38]))),
        "u64-range" => Some((
            "unsigned",
            serde_json::from_str("18446744073709551616").unwrap(),
        )),
        "u64-negative" => Some(("unsigned", json!(-1))),
        _ => None,
    };
    if let Some((key, value)) = mutation {
        structured.insert(key.into(), value);
    }
    let mut metadata = json!({
        "trace": "kept metadata",
        "large": "RAW_LARGE",
        "tiny": "RAW_TINY",
        "literal": {"$serde_json::private::Number": "7"}
    });
    if matches!(scenario, "depth-maximum" | "depth-overflow") {
        let mut nested = json!(0);
        for _ in 0..if scenario == "depth-maximum" {
            125
        } else {
            126
        } {
            nested = Value::Array(vec![nested]);
        }
        metadata
            .as_object_mut()
            .unwrap()
            .insert("nested".into(), nested);
    }
    let mut result = json!({
        "content": [{"type": "text", "text": "kept content"}],
        "structuredContent": structured,
        "_meta": metadata
    });
    let fields = result.as_object_mut().unwrap();
    match scenario {
        "tool-error" => {
            fields.insert("isError".into(), json!(true));
            fields.insert(
                "structuredContent".into(),
                json!({"invalid": "success schema must not be decoded"}),
            );
        }
        "missing-structured" => {
            fields.remove("structuredContent");
        }
        "envelope-invalid" => {
            fields.insert("isError".into(), json!("false"));
        }
        _ => {}
    }
    let mut rpc = json!({"jsonrpc": "2.0", "id": message.get("id"), "result": result});
    let fields = rpc.as_object_mut().unwrap();
    if scenario == "rpc-error" {
        fields.remove("result");
        fields.insert(
            "error".into(),
            json!({
                "code": -32602,
                "message": format!("{TOKEN} {SESSION}"),
                "data": {"field": "name"}
            }),
        );
    }
    if scenario == "wrong-id" {
        fields.insert("id".into(), json!("wrong"));
    }
    let mut data = rpc.to_string();
    if scenario != "rpc-error" {
        // Keep the exact wire numbers, including values outside machine ranges.
        replace_wire(&mut data, "\"RAW_LARGE\"", "18446744073709551617");
        replace_wire(&mut data, "\"RAW_TINY\"", "1e-999");
    }
    let spelling = match scenario {
        "integral-decimal" => Some((r#""tiny":-128"#, r#""tiny":-128.0"#)),
        "integral-exponent" => Some((r#""tiny":-128"#, r#""tiny":1e2"#)),
        "integral-negative-zero" => Some((r#""tiny":-128"#, r#""tiny":-0"#)),
        "integral-u64" => Some((
            r#""unsigned":18446744073709551615"#,
            r#""unsigned":18446744073709551615.0"#,
        )),
        "integral-u64-exponent" => Some((
            r#""unsigned":18446744073709551615"#,
            r#""unsigned":18446744073709551615e0"#,
        )),
        "integer-rounded-fraction" => Some((
            r#""tiny":-128"#,
            r#""tiny":-127.9999999999999999999999999999"#,
        )),
        "u64-rounded-fraction" => Some((
            r#""unsigned":18446744073709551615"#,
            r#""unsigned":18446744073709551615.1"#,
        )),
        "integer-exponent-overflow" => Some((r#""tiny":-128"#, r#""tiny":1.28e2"#)),
        "float64-underflow" => Some((r#""double":1.5"#, r#""double":1e-999"#)),
        "float64-overflow" => Some((r#""double":1.5"#, r#""double":1e999"#)),
        "float32-underflow" => Some((r#""floats":[1.5]"#, r#""floats":[1e-999]"#)),
        "float32-cast-underflow" => Some((r#""floats":[1.5]"#, r#""floats":[1e-90]"#)),
        "zero-exponent" => Some((r#""double":1.5"#, r#""double":0e-999"#)),
        "rpc-id-decimal" => Some((r#""id":2"#, r#""id":2.0"#)),
        "rpc-id-exponent" => Some((r#""id":2"#, r#""id":2e0"#)),
        "rpc-id-fractional" => Some((r#""id":2"#, r#""id":2.5"#)),
        "rpc-id-string" => Some((r#""id":2"#, r#""id":"2""#)),
        "duplicate-wire-key" => Some((r#""tiny":-128"#, r#""tiny":1,"tiny":2"#)),
        _ => None,
    };
    if let Some((old, new)) = spelling {
        replace_wire(&mut data, old, new);
    }
    data
}

#[derive(Debug)]
struct Release(Arc<AtomicBool>);

impl Drop for Release {
    fn drop(&mut self) {
        self.0.store(true, Ordering::SeqCst);
    }
}

#[derive(Debug)]
struct Fixture {
    server: Server,
    state: Arc<Mutex<State>>,
    invocations: usize,
}

#[derive(Debug)]
struct Invocation {
    text: String,
    state: State,
}

impl Invocation {
    fn arguments(&self) -> &Value {
        self.state
            .calls
            .first()
            .and_then(|call| call.get("arguments"))
            .expect("the fixture must receive a tools/call with arguments")
    }
}

impl Fixture {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(State::new("json", false)));
        let handler_state = Arc::clone(&state);
        let server = Server::new(move |request| handler_state.lock().unwrap().handle(request));
        Self {
            server,
            state,
            invocations: 0,
        }
    }

    fn invoke(
        &mut self,
        client: &Client,
        scenario: &str,
        function: Option<&str>,
        expected: &str,
        calls: usize,
    ) -> Invocation {
        let release = {
            let mut state = self.state.lock().unwrap();
            state.reset(scenario, client.authenticated);
            Arc::clone(&state.release)
        };
        let _release = Release(release);
        let output = common::run(
            command(common::wasmtime())
                .args(["run", "-S", "http", "--allow-precompiled", "--invoke"])
                .arg(function.map_or_else(|| format!("typed({BASE})"), str::to_owned))
                .arg(&client.binary),
        );
        let text = String::from_utf8_lossy(&output.stdout).trim().to_owned();
        let state = self.state.lock().unwrap().clone();
        assert!(
            text.starts_with(expected),
            "{scenario}: expected {expected:?}, got {text:?}; requests: {:?}; errors: {:?}; stderr: {}",
            state.requests,
            state.errors,
            String::from_utf8_lossy(&output.stderr)
        );
        self.server.assert_healthy();
        assert!(state.errors.is_empty(), "{scenario}: {:?}", state.errors);
        assert_eq!(state.calls.len(), calls, "{scenario}: {:?}", state.calls);
        if scenario == "sse" {
            assert!(state.opened.load(Ordering::SeqCst), "SSE was not held open");
            assert!(
                !state.release.load(Ordering::SeqCst),
                "SSE completed only after EOF"
            );
            assert_eq!(
                state
                    .requests
                    .iter()
                    .filter(|method| *method == "server-response")
                    .count(),
                2,
                "{:?}",
                state.requests
            );
        }
        if scenario == "expired" {
            assert_eq!(
                state.requests,
                ["initialize", "notifications/initialized", "tools/call"]
            );
        } else if calls == 1 {
            for method in ["tools/call", "DELETE"] {
                assert_eq!(
                    state
                        .requests
                        .iter()
                        .filter(|request| request.as_str() == method)
                        .count(),
                    1,
                    "{scenario}: {:?}",
                    state.requests
                );
            }
        }
        self.invocations += 1;
        println!(
            "PASS {scenario}{}",
            if client.authenticated {
                " (bearer)"
            } else {
                ""
            }
        );
        Invocation { text, state }
    }
}

#[derive(Debug)]
struct Client {
    binary: PathBuf,
    authenticated: bool,
}

fn command(program: impl AsRef<OsStr>) -> Command {
    let mut command = Command::new(program);
    command.current_dir(common::root());
    command
}

fn build_client(base: &Path, cache: &Path, endpoint: &str, authenticated: bool) -> PathBuf {
    let label = if authenticated {
        "authenticated"
    } else {
        "anonymous"
    };
    let destination = base.join(label);
    let unique = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_nanos();
    let name = format!("runtime-{label}-n{unique}-p{}", std::process::id());
    let package = PackageName::parse(&format!("fixture:{name}@0.1.0")).unwrap();
    let generated = generate(
        &parse_catalog(include_str!("fixtures/runtime.json")).unwrap(),
        &package,
        &GenerateOptions {
            endpoint: endpoint.into(),
            bearer_secret: authenticated.then(|| "mcp-test-token".into()),
            limits: Limits {
                request_timeout_ms: 400,
                max_response_bytes: 2048,
                max_event_bytes: 1024,
            },
        },
    )
    .unwrap();
    let mut files = BTreeMap::from([
        (PathBuf::from("wit/world.wit"), generated.wit),
        (PathBuf::from("Cargo.toml"), generated.cargo_toml),
        (PathBuf::from("wasm.toml"), generated.wasm_toml),
        (PathBuf::from("README.md"), generated.readme),
    ]);
    for source in generated.rust {
        files.insert(Path::new("src").join(source.path), source.contents);
    }
    for dependency in generated.wit_dependencies {
        files.insert(Path::new("wit").join(dependency.path), dependency.contents);
    }
    artifact_writer::write_crate(&destination, &files).unwrap();
    common::run(
        common::cargo()
            .args(["build", "--quiet", "--manifest-path"])
            .arg(destination.join("Cargo.toml"))
            .args(["--target", "wasm32-wasip2", "--target-dir"])
            .arg(cache),
    );
    let artifact = cache
        .join("wasm32-wasip2/debug")
        .join(format!("{}.wasm", name.replace('-', "_")));
    common::run(command("wasm-tools").arg("validate").arg(&artifact));
    let output = common::run(
        command("wasm-tools")
            .args(["component", "wit"])
            .arg(&artifact),
    );
    let wit = String::from_utf8_lossy(&output.stdout);
    assert!(
        wit.contains("typed: func") && wit.contains("envelope: func"),
        "{wit}"
    );
    assert_eq!(
        wit.contains("wasmcloud:secrets"),
        authenticated,
        "unexpected secrets imports: {wit}"
    );
    let binary = destination.join("client.wasm");
    fs::copy(artifact, &binary).unwrap();
    binary
}

fn build_clients(base: &Path, endpoint: &str) -> (Client, Client) {
    let cache = common::root().join("target/mcp-component-tests");
    fs::create_dir_all(&cache).unwrap();
    let anonymous = build_client(base, &cache, endpoint, false);
    let anonymous_compiled = base.join("anonymous.cwasm");
    common::run(
        command(common::wasmtime())
            .arg("compile")
            .arg(anonymous)
            .arg("-o")
            .arg(&anonymous_compiled),
    );
    let authenticated = build_client(base, &cache, endpoint, true);
    common::run(
        common::cargo()
            .args(["build", "--quiet", "--manifest-path"])
            .arg(common::crate_dir().join("tests/secrets/Cargo.toml"))
            .args(["--target", "wasm32-wasip2", "--target-dir"])
            .arg(&cache),
    );
    let composed = base.join("composed.wasm");
    common::run(
        command("wac")
            .args(["plug", "--plug"])
            .arg(cache.join("wasm32-wasip2/debug/mcp_test_secrets.wasm"))
            .arg(authenticated)
            .arg("-o")
            .arg(&composed),
    );
    common::run(command("wasm-tools").arg("validate").arg(&composed));
    let authenticated_compiled = base.join("auth.cwasm");
    common::run(
        command(common::wasmtime())
            .arg("compile")
            .arg(composed)
            .arg("-o")
            .arg(&authenticated_compiled),
    );
    (
        Client {
            binary: anonymous_compiled,
            authenticated: false,
        },
        Client {
            binary: authenticated_compiled,
            authenticated: true,
        },
    )
}

fn assert_raw_metadata(output: &str) {
    assert!(output.contains("18446744073709551617"), "{output}");
    assert!(output.contains("1e-999"), "{output}");
}

#[test]
#[ignore = "requires wasm32-wasip2, wasm-tools, wac, and wasmtime"]
fn generated_components_exercise_wasi_http() {
    let temporary = TempDir::new("component-runtime");
    let mut fixture = Fixture::new();
    let (anonymous, authenticated) = build_clients(temporary.path(), &fixture.server.endpoint());

    let output = fixture.invoke(&anonymous, "json", None, "ok(", 1);
    assert_eq!(
        output.arguments(),
        &json!({
            "mode": "green",
            "tiny": -128,
            "double": 1.5,
            "empty": {},
            "map": {"a": 32767},
            "mixed": {"fixed": "value", "extra": true},
            "floats": [1.5],
            "unsigned": u64::MAX,
            "array": [null, "text"]
        })
    );
    assert!(
        output.text.contains("structured-content:"),
        "{}",
        output.text
    );
    assert!(output.text.contains("kept metadata"), "{}", output.text);
    assert_raw_metadata(&output.text);
    assert!(output.text.contains("$serde_json::private::Number"));
    let output = fixture.invoke(&anonymous, "sse", None, "ok(", 1);
    assert_raw_metadata(&output.text);
    fixture.invoke(&authenticated, "json", None, "ok(", 1);
    fixture.invoke(&authenticated, "sse", None, "ok(", 1);
    let output = fixture.invoke(&anonymous, "json", Some("envelope()"), "ok(", 1);
    assert!(
        output.text.contains("kept content") && output.text.contains("kept metadata"),
        "{}",
        output.text
    );
    assert_raw_metadata(&output.text);

    for (scenario, typed) in [
        ("integral-decimal", "tiny: -128"),
        ("integral-exponent", "tiny: 100"),
        ("integral-negative-zero", "tiny: 0"),
        ("integral-u64", "unsigned: some(18446744073709551615)"),
        (
            "integral-u64-exponent",
            "unsigned: some(18446744073709551615)",
        ),
        ("zero-exponent", "double: some(0)"),
        ("rpc-id-decimal", "tiny: -128"),
        ("rpc-id-exponent", "tiny: -128"),
        ("depth-maximum", "tiny: -128"),
    ] {
        let output = fixture.invoke(&anonymous, scenario, None, "ok(", 1);
        assert!(output.text.contains(typed), "{scenario}: {}", output.text);
    }
    for (wave, value) in [
        ("none", None),
        ("some(none)", Some(Value::Null)),
        ("some(some(yes))", Some(json!("yes"))),
    ] {
        let arguments = BASE.replace("nullable: none", &format!("nullable: {wave}"));
        let output = fixture.invoke(
            &anonymous,
            "json",
            Some(&format!("typed({arguments})")),
            "ok(",
            1,
        );
        assert_eq!(output.arguments().get("nullable"), value.as_ref(), "{wave}");
        if wave == "none" {
            assert!(!output.text.contains("nullable:"), "{}", output.text);
        } else {
            assert!(
                output.text.contains(&format!("nullable: {wave}")),
                "{}",
                output.text
            );
        }
    }

    for scenario in [
        "missing-required",
        "wrong-type",
        "integer-overflow",
        "integer-underflow",
        "fractional-integer",
        "enum",
        "closed-object",
        "nonnull",
        "map-type",
        "map-range",
        "mixed-type",
        "float-range",
        "u64-range",
        "u64-negative",
        "missing-structured",
        "integer-rounded-fraction",
        "u64-rounded-fraction",
        "integer-exponent-overflow",
        "float64-underflow",
        "float64-overflow",
        "float32-underflow",
        "float32-cast-underflow",
    ] {
        fixture.invoke(&anonymous, scenario, None, "err(result-decoding(", 1);
    }
    for scenario in [
        "wrong-id",
        "expired",
        "truncated",
        "body-limit",
        "event-limit",
        "sse-body-limit",
        "duplicate-wire-key",
        "envelope-invalid",
        "rpc-id-fractional",
        "rpc-id-string",
        "depth-overflow",
    ] {
        fixture.invoke(&anonymous, scenario, None, "err(protocol(", 1);
    }
    let output = fixture.invoke(&authenticated, "rpc-error", None, "err(protocol(", 1);
    assert!(!output.text.contains(TOKEN) && !output.text.contains(SESSION));
    assert!(output.text.contains("-32602") && output.text.contains("field"));
    let output = fixture.invoke(&anonymous, "tool-error", None, "err(tool-execution(", 1);
    assert!(output.text.contains("kept metadata"), "{}", output.text);
    assert_raw_metadata(&output.text);
    for scenario in ["timeout", "interrupted", "delete-error"] {
        fixture.invoke(&anonymous, scenario, None, "err(transport(", 1);
    }
    for scenario in [
        "version",
        "capability",
        "session-id",
        "ack-status",
        "ack-body",
    ] {
        fixture.invoke(&anonymous, scenario, None, "err(protocol(", 0);
    }
    for scenario in ["http-error", "redirect"] {
        let output = fixture.invoke(&authenticated, scenario, None, "err(transport(", 0);
        assert!(!output.text.contains(TOKEN));
        assert_eq!(output.state.requests, ["initialize"]);
    }
    for arguments in [
        BASE.replace("[(\"a\", 32767)]", "[(\"a\", 1), (\"a\", 2)]"),
        BASE.replace("[(\"extra\", true)]", "[(\"fixed\", true)]"),
        BASE.replace(
            "[(\"extra\", true)]",
            "[(\"extra\", true), (\"extra\", false)]",
        ),
        BASE.replace("[1.5]", "[nan]"),
        BASE.replace("[1.5]", "[inf]"),
    ] {
        let output = fixture.invoke(
            &anonymous,
            "json",
            Some(&format!("typed({arguments})")),
            "err(result-decoding(",
            0,
        );
        assert!(
            output.state.requests.is_empty(),
            "invalid input performed network I/O: {:?}",
            output.state.requests
        );
    }
    assert_eq!(fixture.invocations, 67, "every legacy WASI case must run");
    fixture.server.assert_healthy();
}
