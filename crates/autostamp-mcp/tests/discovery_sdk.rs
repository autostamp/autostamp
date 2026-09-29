//! Official-SDK native discovery against bounded, real loopback HTTP fixtures.
#![cfg(all(feature = "cli", not(target_arch = "wasm32")))]

mod common;

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    time::{Duration, Instant},
};

use autostamp_mcp::{
    discovery::{DiscoveryLimits, discover},
    native::NativeTransport,
    runtime::{Limits, wire},
};
use common::{PROTOCOL, Reply, SESSION, Server, Stream, TOKEN};
use serde_json::{Value, json};

const PAGE: &str = r#"{"tools":[{"name":"raw","description":null,"inputSchema":{"type":"object","additionalProperties":false,"properties":{"count":{"type":"integer","maximum":18446744073709551617}}},"_meta":{"tiny":1e-999,"literal":{"$serde_json::private::Number":"7"},"autostamp/raw-tools-page":"not a capsule"}}]}"#;

fn expected_page() -> Value {
    let json = format!(r#"{{"jsonrpc":"2.0","id":1,"result":{PAGE}}}"#);
    let wire::RpcMessage::Success { result, .. } =
        wire::decode_json(json.as_bytes(), 4096).unwrap()
    else {
        panic!("fixture page must be an object");
    };
    Value::Object(result)
}

struct Fixture {
    server: Server,
    methods: Arc<Mutex<Vec<String>>>,
    release: Arc<AtomicBool>,
    opened: Arc<AtomicBool>,
}

impl Fixture {
    fn new(scenario: &'static str) -> Self {
        let methods = Arc::new(Mutex::new(Vec::new()));
        let release = Arc::new(AtomicBool::new(false));
        let opened = Arc::new(AtomicBool::new(false));
        let received = methods.clone();
        let released = release.clone();
        let stream_opened = opened.clone();
        let server = Server::new(move |request| {
            assert_eq!(
                request.header("Authorization"),
                Some(format!("Bearer {TOKEN}").as_str())
            );
            assert_eq!(
                request.header("Accept"),
                Some("application/json, text/event-stream")
            );
            let message: Value = if request.method == "DELETE" {
                Value::Null
            } else {
                serde_json::from_slice(&request.body).unwrap()
            };
            let method = if request.method == "DELETE" {
                "DELETE"
            } else {
                message
                    .get("method")
                    .and_then(Value::as_str)
                    .unwrap_or("server-response")
            };
            received.lock().unwrap().push(method.into());
            assert_eq!(
                request.header("MCP-Session-Id"),
                (method != "initialize").then_some(SESSION)
            );
            if method != "initialize" {
                assert_eq!(request.header("MCP-Protocol-Version"), Some(PROTOCOL));
            }
            if method == "DELETE" {
                return Reply::empty(if scenario == "cleanup" { 500 } else { 204 });
            }
            if method == "notifications/initialized" {
                return Reply::empty(202);
            }
            if method == "server-response" {
                match message.get("id").and_then(Value::as_str).unwrap() {
                    "server-ping" => assert_eq!(message.get("result"), Some(&json!({}))),
                    "unsupported" => {
                        assert_eq!(message.pointer("/error/code"), Some(&json!(-32601)))
                    }
                    _ => panic!("unexpected server-request response"),
                }
                return Reply::empty(202);
            }
            assert!(
                matches!(method, "initialize" | "tools/list"),
                "discovery invoked a tool"
            );
            let id = message.get("id").unwrap().to_string();
            let mut reply = if method == "initialize" {
                assert_eq!(message.pointer("/params/capabilities"), Some(&json!({})));
                let mut reply = Reply::json(
                    200,
                    json!({
                        "jsonrpc":"2.0", "id":message.get("id").unwrap(),
                        "result":{"protocolVersion":PROTOCOL,"capabilities":{"tools":{}},
                        "serverInfo":{"name":"fixture","version":"1"}}
                    })
                    .to_string()
                    .into_bytes(),
                );
                reply
                    .headers
                    .push(("MCP-Session-Id".into(), SESSION.into()));
                reply
            } else {
                let body = match scenario {
                    "wrapper" => format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"autostamp/raw-tools-page":"{{\"tools\":[]}}"}}}}"#),
                    "duplicate" => format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[],"tools":[]}}}}"#),
                    "rpc-error" => json!({"jsonrpc":"2.0","id":message.get("id").unwrap(),"error":{
                        "code":-32602,"message":format!("{TOKEN} {SESSION}"),"data":{"field":"inputSchema"}
                    }}).to_string(),
                    "decimal-id" => format!(r#"{{"jsonrpc":"2.0","id":{id}.0,"result":{PAGE}}}"#),
                    "wrong-id" => format!(r#"{{"jsonrpc":"2.0","id":"wrong","result":{PAGE}}}"#),
                    "session-echo" => format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{"tools":[{{"name":"echo","description":"{SESSION}"}}]}}}}"#),
                    _ => format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{PAGE}}}"#),
                };
                Reply::json(200, body.into_bytes())
            };
            if scenario.starts_with("sse") {
                reply
                    .headers
                    .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
                reply
                    .headers
                    .push(("Content-Type".into(), "text/event-stream".into()));
                let mut event = b": priming\r\n\r\n".to_vec();
                if scenario == "sse-controls" && method == "tools/list" {
                    event.extend_from_slice(b"data: {\"jsonrpc\":\"2.0\",\"id\":\"server-ping\",\"method\":\"ping\"}\r\n\r\n");
                    event.extend_from_slice(b"data: {\"jsonrpc\":\"2.0\",\"id\":\"unsupported\",\"method\":\"not/supported\"}\r\n\r\n");
                }
                event.extend_from_slice(b"data: ");
                event.extend_from_slice(&reply.body);
                event.extend_from_slice(b"\r\n\r\ndata: NEVER READ");
                let chunks = if scenario == "sse-timeout" && method == "tools/list" {
                    vec![b": still active\n\n".to_vec(); 30]
                } else {
                    event.chunks(7).map(<[u8]>::to_vec).collect()
                };
                reply.stream = Some(Stream {
                    gate_before_chunk: None,
                    chunks,
                    interval: if scenario == "sse-timeout" && method == "tools/list" {
                        Duration::from_millis(25)
                    } else {
                        Duration::ZERO
                    },
                    release: Some(released.clone()),
                    opened: Some(stream_opened.clone()),
                    terminate: true,
                });
            }
            reply
        });
        Self {
            server,
            methods,
            release,
            opened,
        }
    }

    async fn discover(&self) -> Result<Value, autostamp_mcp::runtime::Error> {
        discover(
            NativeTransport::new(13).unwrap(),
            self.server.endpoint(),
            Some(TOKEN.into()),
            Limits {
                request_timeout_ms: 300,
                max_response_bytes: 4096,
                max_event_bytes: 1024,
            },
            DiscoveryLimits::default(),
        )
        .await
    }
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.release.store(true, Ordering::SeqCst);
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_json_and_sse_preserve_exact_raw_catalogs() {
    for scenario in ["json", "sse", "decimal-id"] {
        let fixture = Fixture::new(scenario);
        let catalog = fixture.discover().await.unwrap();
        assert_eq!(catalog, expected_page(), "{scenario}");
        assert_eq!(
            fixture.methods.lock().unwrap().as_slice(),
            [
                "initialize",
                "notifications/initialized",
                "tools/list",
                "DELETE"
            ]
        );
        if scenario == "sse" {
            assert!(fixture.opened.load(Ordering::SeqCst));
            assert!(!fixture.release.load(Ordering::SeqCst));
        }
        fixture.server.assert_healthy();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_dispatches_inline_server_requests() {
    let fixture = Fixture::new("sse-controls");
    assert_eq!(fixture.discover().await.unwrap(), expected_page());
    assert_eq!(
        fixture
            .methods
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "server-response")
            .count(),
        2
    );
    fixture.server.assert_healthy();
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_failures_remain_explicit_and_credential_safe() {
    for (scenario, expected) in [
        ("wrapper", "tools array"),
        ("duplicate", "invalid"),
        ("rpc-error", "-32602"),
        ("cleanup", "HTTP status 500"),
        ("wrong-id", "transport:"),
        ("session-echo", "credential"),
    ] {
        let fixture = Fixture::new(scenario);
        let error = fixture.discover().await.unwrap_err().to_string();
        assert!(error.contains(expected), "{scenario}: {error}");
        assert!(!error.contains(TOKEN) && !error.contains(SESSION));
        fixture.server.assert_healthy();
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_stream_progress_does_not_extend_deadline() {
    let fixture = Fixture::new("sse-timeout");
    let start = Instant::now();
    assert!(fixture.discover().await.is_err());
    assert!(start.elapsed() < Duration::from_secs(2));
    assert_eq!(
        fixture
            .methods
            .lock()
            .unwrap()
            .iter()
            .filter(|method| *method == "tools/list")
            .count(),
        1
    );
}

async fn lifecycle_case(
    scenario: &'static str,
) -> (
    Result<Value, autostamp_mcp::runtime::Error>,
    Vec<String>,
    Duration,
) {
    let methods = Arc::new(Mutex::new(Vec::new()));
    let received = methods.clone();
    let ping_received = Arc::new(AtomicBool::new(false));
    let server = Server::new(move |request| {
        let message: Value = if request.method == "DELETE" {
            Value::Null
        } else {
            serde_json::from_slice(&request.body).unwrap()
        };
        let method = if request.method == "DELETE" {
            "DELETE"
        } else {
            message
                .get("method")
                .and_then(Value::as_str)
                .unwrap_or("server-response")
        };
        received.lock().unwrap().push(method.into());
        match method {
            "DELETE" => {
                return Reply::empty(if scenario == "bootstrap-cleanup-failure" {
                    500
                } else {
                    204
                });
            }
            "notifications/initialized" => {
                if scenario == "bootstrap-timeout" {
                    std::thread::sleep(Duration::from_millis(800));
                }
                return Reply::empty(
                    if scenario.starts_with("bootstrap-fail")
                        || scenario == "bootstrap-cleanup-failure"
                    {
                        500
                    } else {
                        202
                    },
                );
            }
            "server-response" => {
                assert_eq!(message.get("id"), Some(&json!("bootstrap-ping")));
                assert_eq!(message.get("result"), Some(&json!({})));
                if scenario == "slow-control" {
                    std::thread::sleep(Duration::from_millis(2400));
                } else if scenario == "control-timeout" {
                    std::thread::sleep(Duration::from_millis(800));
                }
                ping_received.store(true, Ordering::SeqCst);
                return Reply::empty(202);
            }
            "initialize" | "tools/list" => {}
            _ => panic!("unexpected SDK lifecycle method {method}"),
        }
        let result = if method == "initialize" {
            json!({"protocolVersion":PROTOCOL,"capabilities":{"tools":{}},"serverInfo":{"name":"fixture","version":"1"}})
        } else {
            json!({"tools":[]})
        };
        let data =
            json!({"jsonrpc":"2.0","id":message.get("id").unwrap(),"result":result}).to_string();
        let mut reply = Reply::json(200, data.as_bytes().to_vec());
        if method == "initialize" {
            reply
                .headers
                .push(("MCP-Session-Id".into(), SESSION.into()));
        }
        let bootstrap_ping = scenario == "bootstrap-ping" && method == "initialize";
        if bootstrap_ping
            || (matches!(scenario, "slow-control" | "control-timeout") && method == "tools/list")
        {
            reply
                .headers
                .retain(|(name, _)| !name.eq_ignore_ascii_case("content-type"));
            reply
                .headers
                .push(("Content-Type".into(), "text/event-stream".into()));
            reply.stream = Some(Stream {
                chunks: vec![
                    b"data: {\"jsonrpc\":\"2.0\",\"id\":\"bootstrap-ping\",\"method\":\"ping\"}\n\n".to_vec(),
                    format!("data: {data}\n\n").into_bytes(),
                ],
                gate_before_chunk: bootstrap_ping.then(|| (1, ping_received.clone())),
                interval: Duration::ZERO, release: None, opened: None, terminate: true,
            });
        }
        reply
    });
    let start = Instant::now();
    let result = discover(
        NativeTransport::new(23).unwrap(),
        server.endpoint(),
        None,
        Limits {
            request_timeout_ms: if scenario == "slow-control" {
                4000
            } else {
                300
            },
            max_response_bytes: 4096,
            max_event_bytes: 1024,
        },
        DiscoveryLimits::default(),
    )
    .await;
    let elapsed = start.elapsed();
    server.assert_healthy();
    drop(server);
    let methods = methods.lock().unwrap().clone();
    (result, methods, elapsed)
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_bootstrap_answers_ping_before_initialize_result() {
    let (result, methods, _) = lifecycle_case("bootstrap-ping").await;
    assert_eq!(result.unwrap(), json!({"tools":[]}));
    assert_eq!(
        methods,
        [
            "initialize",
            "server-response",
            "notifications/initialized",
            "tools/list",
            "DELETE"
        ]
    );
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_failed_or_timed_out_bootstrap_releases_allocated_session() {
    for scenario in [
        "bootstrap-failure",
        "bootstrap-timeout",
        "bootstrap-cleanup-failure",
    ] {
        let (result, methods, elapsed) = lifecycle_case(scenario).await;
        let error = result.unwrap_err().to_string();
        assert!(elapsed < Duration::from_secs(2), "{scenario}: {elapsed:?}");
        assert_eq!(
            methods,
            ["initialize", "notifications/initialized", "DELETE"],
            "{scenario}: {error}"
        );
        if scenario == "bootstrap-cleanup-failure" {
            assert!(error.contains("cleanup"), "{error}");
        }
    }
}

#[tokio::test(flavor = "current_thread")]
async fn sdk_drains_control_acknowledgements_using_the_configured_deadline() {
    let (result, methods, elapsed) = lifecycle_case("slow-control").await;
    assert_eq!(result.unwrap(), json!({"tools":[]}));
    assert!(
        elapsed >= Duration::from_millis(2400) && elapsed < Duration::from_secs(4),
        "{elapsed:?}"
    );
    assert_eq!(
        methods,
        [
            "initialize",
            "notifications/initialized",
            "tools/list",
            "server-response",
            "DELETE"
        ]
    );

    let (result, methods, elapsed) = lifecycle_case("control-timeout").await;
    assert!(result.is_err());
    assert!(elapsed < Duration::from_secs(2), "{elapsed:?}");
    assert_eq!(
        methods
            .iter()
            .filter(|method| *method == "server-response")
            .count(),
        1
    );
}
