//! Mock-only CLI discovery, standalone component builds, and explicit WASI calls.
#![cfg(all(feature = "cli", not(target_arch = "wasm32")))]

mod common;

use std::{
    fs,
    path::{Path, PathBuf},
    process::{Command, Output},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::Duration,
};

use common::{PROTOCOL, Reply, Request, SESSION, Server, Stream, TOKEN, TempDir};
use serde_json::{Value, json};

fn tools() -> Vec<Value> {
    ["ping", "pong"]
        .into_iter()
        .map(|name| {
            json!({
                "name": name,
                "description": format!("Call {name}."),
                "inputSchema": {"type":"object", "additionalProperties":false}
            })
        })
        .collect()
}

struct State {
    scenario: String,
    token: Option<String>,
    requests: Vec<String>,
    calls: Vec<Value>,
    release: Arc<AtomicBool>,
    opened: Arc<AtomicBool>,
}

impl State {
    fn new(scenario: &str, token: Option<&str>) -> Self {
        Self {
            scenario: scenario.into(),
            token: token.map(str::to_owned),
            requests: Vec::new(),
            calls: Vec::new(),
            release: Arc::new(AtomicBool::new(false)),
            opened: Arc::new(AtomicBool::new(false)),
        }
    }

    fn rpc(&self, message: &Value, result: Value, initialize: bool) -> Reply {
        let data =
            json!({"jsonrpc":"2.0", "id":message.get("id").unwrap(), "result":result}).to_string();
        let mut reply = Reply::json(200, data.as_bytes().to_vec());
        if initialize {
            reply
                .headers
                .push(("MCP-Session-Id".into(), SESSION.into()));
        }
        if self.scenario == "sse" {
            reply
                .headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
            reply
                .headers
                .push(("Content-Type".into(), "text/event-stream".into()));
            let event = format!(": comment\r\nevent: message\r\ndata: {data}\r\n\r\n");
            reply.stream = Some(Stream {
                gate_before_chunk: None,
                chunks: event.as_bytes().chunks(7).map(<[u8]>::to_vec).collect(),
                interval: Duration::ZERO,
                release: Some(self.release.clone()),
                opened: Some(self.opened.clone()),
                terminate: true,
            });
        }
        reply
    }
}

fn handle(request: Request, state: &Mutex<State>) -> Reply {
    let mut state = state.lock().unwrap();
    let closing = request.method == "DELETE";
    let message: Value = if closing {
        Value::Null
    } else {
        serde_json::from_slice(&request.body).unwrap()
    };
    let method = if closing {
        "DELETE"
    } else {
        message.get("method").unwrap().as_str().unwrap()
    };
    let initialized = method != "initialize";
    state.requests.push(method.into());
    let authorization = state.token.as_ref().map(|token| format!("Bearer {token}"));
    assert_eq!(
        request.header("Authorization"),
        authorization.as_deref(),
        "authorization"
    );
    assert_eq!(
        request.header("MCP-Session-Id"),
        initialized.then_some(SESSION),
        "session"
    );
    assert_eq!(
        request.header("Accept"),
        Some("application/json, text/event-stream")
    );
    if closing {
        assert!(matches!(
            request.header("MCP-Protocol-Version"),
            None | Some(PROTOCOL)
        ));
        return Reply::empty(if state.scenario == "cleanup" {
            500
        } else {
            204
        });
    }
    assert_eq!(
        request.header("MCP-Protocol-Version"),
        initialized.then_some(PROTOCOL)
    );
    match method {
        "initialize" => {
            assert_eq!(message.pointer("/params/capabilities"), Some(&json!({})));
            if state.scenario == "auth" {
                return Reply::json(401, format!("Never print {TOKEN} {SESSION}").into_bytes());
            }
            state.rpc(&message, json!({
                "protocolVersion": if state.scenario == "version" { "future" } else { PROTOCOL },
                "capabilities": if state.scenario == "capability" { json!({}) } else { json!({"tools":{}}) },
                "serverInfo":{"name":"fixture", "version":"1"}
            }), true)
        }
        "notifications/initialized" => Reply::empty(202),
        "tools/list" => {
            let cursor = message.pointer("/params/cursor");
            assert!(
                cursor.is_none() || cursor == Some(&json!("")),
                "opaque empty cursor"
            );
            if state.scenario == "malformed" {
                return Reply::json(200, b"invalid".to_vec());
            }
            if state.scenario == "rpc" {
                return Reply::json(200, json!({
                    "jsonrpc":"2.0", "id":message.get("id").unwrap(),
                    "error":{"code":-32602, "message":format!("{TOKEN} {SESSION} field failure")}
                }).to_string().into_bytes());
            }
            let tools = tools();
            let mut page = if cursor.is_none() {
                json!({"tools":[tools.get(1).unwrap()], "nextCursor":""})
            } else {
                json!({"tools":[tools.first().unwrap()]})
            };
            match state.scenario.as_str() {
                "duplicate" if cursor.is_some() => page = json!({"tools":[tools.get(1).unwrap()]}),
                "cursor" => page = json!({"tools":[], "nextCursor":""}),
                "null-cursor" => {
                    page.as_object_mut()
                        .unwrap()
                        .insert("nextCursor".into(), Value::Null);
                }
                "bad-page" => page = json!({"tools":null}),
                "empty" => page = json!({"tools":[]}),
                "schema" => {
                    *page.pointer_mut("/tools/0/inputSchema").unwrap() = json!({"type":"object"})
                }
                "echo" => *page.pointer_mut("/tools/0/description").unwrap() = json!(state.token),
                _ => {}
            }
            state.rpc(&message, page, false)
        }
        "tools/call" => {
            let params = message.get("params").unwrap();
            assert_eq!(params, &json!({"name":"ping", "arguments":{}}));
            state.calls.push(params.clone());
            state.rpc(
                &message,
                json!({"content":[{"type":"text", "text":"explicit invocation only"}]}),
                false,
            )
        }
        _ => panic!("unexpected method {method}"),
    }
}

struct Fixture {
    server: Server,
    state: Arc<Mutex<State>>,
    directory: TempDir,
    cache: PathBuf,
}

impl Fixture {
    fn new() -> Self {
        let state = Arc::new(Mutex::new(State::new("json", None)));
        let server_state = state.clone();
        let server = Server::new(move |request| handle(request, &server_state));
        Self {
            server,
            state,
            directory: TempDir::new("cli"),
            cache: common::root().join("target/mcp-cli-tests"),
        }
    }

    fn reset(&self, scenario: &str, token: Option<&str>) {
        let mut state = self.state.lock().unwrap();
        state.release.store(true, Ordering::SeqCst);
        *state = State::new(scenario, token);
    }

    fn path(&self, name: &str) -> PathBuf {
        self.directory.path().join(name)
    }

    fn cli(
        &self,
        destination: &Path,
        args: &[&str],
        catalog: Option<&Path>,
        expected: i32,
    ) -> Output {
        let token = self.state.lock().unwrap().token.clone();
        let mut command = Command::new(env!("CARGO_BIN_EXE_autostamp-mcp"));
        command
            .current_dir(common::root())
            .env_remove("AUTOSTAMP_CLI_TEST_TOKEN");
        if let Some(catalog) = catalog {
            command
                .arg("--catalog")
                .arg(catalog)
                .arg("--endpoint")
                .arg(self.server.endpoint());
        } else {
            command.arg("--server").arg(self.server.endpoint());
        }
        if let Some(token) = token {
            command
                .env("AUTOSTAMP_CLI_TEST_TOKEN", token)
                .args(["--token-env", "AUTOSTAMP_CLI_TEST_TOKEN"]);
        }
        let name = destination
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace(' ', "-");
        command
            .args([
                "--package",
                &format!("fixture:cli-{name}@0.1.0"),
                "--output",
            ])
            .arg(destination)
            .args(["--timeout-ms", "1000"])
            .args(args);
        let output = common::output(&mut command);
        self.server.assert_healthy();
        assert_eq!(
            output.status.code(),
            Some(expected),
            "{}; requests: {:?}",
            String::from_utf8_lossy(&output.stderr),
            self.state.lock().unwrap().requests
        );
        assert_no_credentials(&output.stdout);
        assert_no_credentials(&output.stderr);
        assert!(
            self.state.lock().unwrap().calls.is_empty(),
            "generation invoked a tool"
        );
        self.server.assert_healthy();
        if expected == 0 {
            check_files(destination);
        }
        output
    }

    fn build(&self, destination: &Path) -> (PathBuf, String) {
        common::run(
            common::cargo()
                .args(["build", "--quiet", "--manifest-path"])
                .arg(destination.join("Cargo.toml"))
                .args(["--target", "wasm32-wasip2", "--target-dir"])
                .arg(&self.cache),
        );
        let name = destination
            .file_name()
            .unwrap()
            .to_str()
            .unwrap()
            .replace([' ', '-'], "_");
        let source = self
            .cache
            .join(format!("wasm32-wasip2/debug/cli_{name}.wasm"));
        common::run(Command::new("wasm-tools").arg("validate").arg(&source));
        let wit = common::run(
            Command::new("wasm-tools")
                .args(["component", "wit"])
                .arg(&source),
        );
        let binary = destination.join("client.wasm");
        fs::copy(source, &binary).unwrap();
        (binary, String::from_utf8(wit.stdout).unwrap())
    }

    fn invoke(&self, binary: &Path) {
        let output = common::run(
            Command::new(common::wasmtime())
                .args(["run", "-S", "http", "--invoke", "ping()"])
                .arg(binary),
        );
        let text = String::from_utf8(output.stdout).unwrap();
        assert!(
            text.starts_with("ok(") && text.contains("explicit invocation only"),
            "{text}"
        );
        assert_no_credentials(text.as_bytes());
        assert_eq!(self.state.lock().unwrap().calls.len(), 1);
        self.server.assert_healthy();
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .release
            .store(true, Ordering::SeqCst);
    }
}

fn assert_no_credentials(bytes: &[u8]) {
    for secret in [TOKEN, SESSION] {
        assert!(
            !bytes
                .windows(secret.len())
                .any(|window| window == secret.as_bytes()),
            "credential persisted"
        );
    }
}

fn check_files(directory: &Path) {
    for entry in fs::read_dir(directory).unwrap() {
        let path = entry.unwrap().path();
        if path.is_dir() {
            check_files(&path);
        } else {
            assert_no_credentials(&fs::read(path).unwrap());
        }
    }
}

#[test]
#[ignore = "requires wasm32-wasip2, wasm-tools, wac, and wasmtime"]
fn discovery_build_and_explicit_invocation() {
    let fixture = Fixture::new();
    let mut passed = 0;
    let live = fixture.path("live crate");
    for scenario in ["json", "sse"] {
        fixture.reset(scenario, None);
        fixture.cli(&live, &[], None, 0);
        let state = fixture.state.lock().unwrap();
        assert_eq!(
            state.requests,
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "tools/list",
                "DELETE"
            ]
        );
        if scenario == "sse" {
            assert!(state.opened.load(Ordering::SeqCst));
            assert!(!state.release.load(Ordering::SeqCst));
        }
        state.release.store(true, Ordering::SeqCst);
        passed += 1;
    }
    let (binary, wit) = fixture.build(&live);
    assert!(wit.contains("ping: func") && wit.contains("pong: func"));
    for scenario in ["json", "sse"] {
        fixture.reset(scenario, None);
        fixture.invoke(&binary);
        assert_eq!(
            fixture.state.lock().unwrap().requests,
            [
                "initialize",
                "notifications/initialized",
                "tools/call",
                "DELETE"
            ]
        );
        passed += 1;
    }

    fixture.reset("json", Some(TOKEN));
    let authenticated = fixture.path("auth");
    fixture.cli(
        &authenticated,
        &["--bearer-secret", "mcp-test-token"],
        None,
        0,
    );
    let (auth_binary, _) = fixture.build(&authenticated);
    common::run(
        common::cargo()
            .args(["build", "--quiet", "--manifest-path"])
            .arg(common::crate_dir().join("tests/secrets/Cargo.toml"))
            .args(["--target", "wasm32-wasip2", "--target-dir"])
            .arg(&fixture.cache),
    );
    let composed = fixture.path("composed.wasm");
    common::run(
        Command::new("wac")
            .args(["plug", "--plug"])
            .arg(
                fixture
                    .cache
                    .join("wasm32-wasip2/debug/mcp_test_secrets.wasm"),
            )
            .arg(auth_binary)
            .arg("-o")
            .arg(&composed),
    );
    common::run(Command::new("wasm-tools").arg("validate").arg(&composed));
    fixture.reset("json", Some(TOKEN));
    fixture.invoke(&composed);
    passed += 1;

    fixture.reset("json", None);
    fs::write(authenticated.join("user.txt"), "keep").unwrap();
    fixture.cli(&authenticated, &[], None, 0);
    assert!(!authenticated.join("wit/deps/wasmcloud-secrets").exists());
    assert_eq!(
        fs::read_to_string(authenticated.join("user.txt")).unwrap(),
        "keep"
    );
    let (_, wit) = fixture.build(&authenticated);
    assert!(!wit.contains("wasmcloud:secrets"));
    passed += 1;

    fixture.reset("json", Some("serde_json"));
    fixture.cli(&fixture.path("template-coincidence"), &[], None, 0);
    passed += 1;

    fixture.reset("json", None);
    let catalog = fixture.path("catalog.json");
    fs::write(&catalog, json!({"tools":tools()}).to_string()).unwrap();
    let offline = fixture.path("offline");
    fixture.cli(&offline, &[], Some(&catalog), 0);
    assert!(fixture.state.lock().unwrap().requests.is_empty());
    assert_eq!(
        fs::read_to_string(offline.join("wit/world.wit"))
            .unwrap()
            .lines()
            .skip(1)
            .collect::<Vec<_>>(),
        fs::read_to_string(live.join("wit/world.wit"))
            .unwrap()
            .lines()
            .skip(1)
            .collect::<Vec<_>>()
    );
    passed += 1;

    fixture.reset("empty", None);
    let empty = fixture.path("empty");
    fixture.cli(&empty, &[], None, 0);
    assert!(
        !fs::read_to_string(empty.join("wit/world.wit"))
            .unwrap()
            .contains(": func(")
    );
    fixture.build(&empty);
    passed += 1;

    let credential_endpoint = format!("{}/{TOKEN}", fixture.server.endpoint());
    let failures: Vec<(&str, Vec<&str>, &str)> = vec![
        ("capability", vec![], "tools capability"),
        ("version", vec![], "unsupported negotiated protocol"),
        ("auth", vec![], "HTTP status 401"),
        ("malformed", vec![], "protocol:"),
        ("bad-page", vec![], "tools array"),
        ("duplicate", vec![], "duplicate tool"),
        ("cursor", vec![], "repeated pagination"),
        ("null-cursor", vec![], "nextCursor must be a string"),
        ("schema", vec![], "/tools/"),
        ("rpc", vec![], "-32602"),
        ("cleanup", vec![], "HTTP status 500"),
        ("echo", vec![], "discovery credential"),
        (
            "json",
            vec!["--bearer-secret", TOKEN],
            "discovery credential",
        ),
        (
            "json",
            vec!["--endpoint", &credential_endpoint],
            "discovery credential",
        ),
        ("json", vec!["--max-pages", "1"], "page limit"),
        ("json", vec!["--max-tools", "1"], "tool limit"),
        (
            "json",
            vec!["--max-catalog-bytes", "12"],
            "catalog byte limit",
        ),
        (
            "json",
            vec!["--max-response-bytes", "100", "--max-event-bytes", "100"],
            "byte limit",
        ),
        ("sse", vec!["--max-event-bytes", "10"], "limit"),
    ];
    for (index, (scenario, flags, expected)) in failures.into_iter().enumerate() {
        fixture.reset(scenario, Some(TOKEN));
        let destination = fixture.path(&format!("failure-case{index}"));
        let result = fixture.cli(&destination, &flags, None, 1);
        assert!(
            String::from_utf8_lossy(&result.stderr).contains(expected),
            "{scenario}: {result:?}"
        );
        assert!(
            !destination.exists(),
            "failed discovery wrote partial output"
        );
        passed += 1;
    }
    assert_eq!(passed, 28);
    println!("All {passed} CLI discovery/build/invoke integration checks passed.");
}
