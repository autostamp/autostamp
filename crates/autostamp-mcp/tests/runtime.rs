//! The same session implementation used by generated WASI clients, with mock I/O.

use std::{
    cell::RefCell,
    collections::VecDeque,
    future::{Future, pending, poll_fn},
    pin::pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use autostamp_mcp::runtime::{
    Body, Error, Limits, Request, Response, Session, Transport, tool_result,
};
use serde_json::{Value, json};

#[derive(Debug)]
struct MockBody {
    chunks: VecDeque<Result<Vec<u8>, Error>>,
    stay_open: bool,
}

impl Body for MockBody {
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if let Some(chunk) = self.chunks.pop_front() {
            chunk.map(Some)
        } else if self.stay_open {
            pending().await
        } else {
            Ok(None)
        }
    }
}

#[derive(Clone, Debug)]
struct Mock {
    responses: Rc<RefCell<VecDeque<Response<MockBody>>>>,
    requests: Rc<RefCell<Vec<Request>>>,
}

impl Transport for Mock {
    type Body = MockBody;

    async fn send(&self, request: Request) -> Result<Response<MockBody>, Error> {
        self.requests.borrow_mut().push(request);
        self.responses
            .borrow_mut()
            .pop_front()
            .ok_or_else(|| Error::Transport("mock response exhausted".into()))
    }

    async fn within<T>(
        &self,
        timeout: Duration,
        future: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        let start = Instant::now();
        let mut future = pin!(future);
        poll_fn(|cx| {
            if start.elapsed() >= timeout {
                Poll::Ready(Err(Error::Transport("total request timeout".into())))
            } else {
                cx.waker().wake_by_ref();
                future.as_mut().poll(cx)
            }
        })
        .await
    }
}

fn run<T>(future: impl Future<Output = T>) -> T {
    let mut context = Context::from_waker(Waker::noop());
    let mut future = pin!(future);
    loop {
        if let Poll::Ready(result) = future.as_mut().poll(&mut context) {
            return result;
        }
        std::thread::yield_now();
    }
}

fn response(status: u16, content_type: &str, bytes: Vec<u8>) -> Response<MockBody> {
    Response {
        status,
        headers: vec![("Content-Type".into(), content_type.into())],
        body: MockBody {
            chunks: [Ok(bytes)].into(),
            stay_open: false,
        },
    }
}

fn rpc(id: u64, result: Value) -> Response<MockBody> {
    response(
        200,
        "application/json; charset=utf-8",
        json!({"jsonrpc":"2.0","id":id,"result":result})
            .to_string()
            .into_bytes(),
    )
}

fn initialized_result() -> Value {
    json!({
        "protocolVersion":"2025-11-25",
        "capabilities":{"tools":{}},
        "serverInfo":{"name":"fixture","version":"1"}
    })
}

fn initialize() -> Response<MockBody> {
    let mut response = rpc(1, initialized_result());
    response
        .headers
        .push(("MCP-Session-Id".into(), "private-session".into()));
    response
}

fn ack() -> Response<MockBody> {
    response(202, "", vec![])
}

fn fixture(responses: Vec<Response<MockBody>>) -> (Mock, Session<Mock>) {
    fixture_with_timeout(responses, 2_000)
}

fn fixture_with_timeout(
    responses: Vec<Response<MockBody>>,
    request_timeout_ms: u64,
) -> (Mock, Session<Mock>) {
    let mock = Mock {
        responses: Rc::new(RefCell::new(responses.into())),
        requests: Rc::new(RefCell::new(Vec::new())),
    };
    let session = Session::new(
        mock.clone(),
        "http://localhost/mcp".into(),
        Some("fixture-token".into()),
        Limits {
            request_timeout_ms,
            ..Limits::default()
        },
    )
    .unwrap();
    (mock, session)
}

#[test]
fn initialize_call_and_close_forward_headers_without_leaking_debug() {
    let result = json!({"content":[],"structuredContent":{"answer":42}});
    let (mock, mut session) = fixture(vec![
        initialize(),
        ack(),
        rpc(2, result.clone()),
        response(405, "", vec![]),
    ]);
    run(async {
        session.initialize().await.unwrap();
        assert_eq!(
            session
                .request("tools/call", json!({"name":"test","arguments":{}}))
                .await
                .unwrap(),
            result
        );
        session.close().await.unwrap();
        session.close().await.unwrap();
        assert!(session.request("tools/list", json!({})).await.is_err());
    });
    let requests = mock.requests.borrow();
    assert_eq!(requests.len(), 4);
    for (index, request) in requests.iter().enumerate() {
        assert!(
            request
                .headers
                .contains(&("authorization".into(), "Bearer fixture-token".into()))
        );
        assert!(request.headers.contains(&(
            "accept".into(),
            "application/json, text/event-stream".into()
        )));
        assert_eq!(
            request
                .headers
                .iter()
                .any(|(key, _)| key == "mcp-session-id"),
            index != 0
        );
        assert_eq!(
            request
                .headers
                .iter()
                .any(|(key, _)| key == "mcp-protocol-version"),
            index != 0
        );
        assert!(!format!("{request:?}").contains("fixture-token"));
        assert!(!format!("{request:?}").contains("private-session"));
    }
    assert_eq!(requests.last().unwrap().method, "DELETE");
    assert!(!format!("{session:?}").contains("fixture-token"));
}

#[test]
fn sse_handles_ping_unsupported_requests_progress_and_stops_before_eof() {
    let mut streamed = response(
        200,
        "text/event-stream",
        concat!(
            ": comment\r\n\r\n",
            "id: 1\r\ndata:\r\n\r\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":\"ping-id\",\"method\":\"ping\"}\r\n\r\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":88,\"method\":\"sampling/createMessage\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/progress\",\"params\":{\"progress\":1}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{\"content\":[]}}\n\n",
            "data: incomplete and never consumed"
        ).as_bytes().to_vec(),
    );
    streamed.body.stay_open = true;
    let (mock, mut session) = fixture(vec![initialize(), ack(), streamed, ack(), ack()]);
    run(async {
        session.initialize().await.unwrap();
        assert_eq!(
            session.request("tools/call", json!({})).await.unwrap(),
            json!({"content":[]})
        );
    });
    let requests = mock.requests.borrow();
    let replies: Vec<Value> = requests
        .iter()
        .skip(3)
        .map(|request| serde_json::from_slice(&request.body).unwrap())
        .collect();
    assert_eq!(replies.first().unwrap()["id"], "ping-id");
    assert_eq!(replies.first().unwrap()["result"], json!({}));
    assert_eq!(
        replies.last().unwrap().pointer("/error/code"),
        Some(&json!(-32601))
    );
}

#[test]
fn expired_session_is_never_reused_or_replayed() {
    let (mock, mut session) = fixture(vec![initialize(), ack(), response(404, "", vec![])]);
    run(async {
        session.initialize().await.unwrap();
        let error = session.request("tools/call", json!({})).await.unwrap_err();
        assert!(matches!(error, Error::Protocol(text) if text.contains("expired")));
        assert!(session.request("tools/call", json!({})).await.is_err());
        assert!(session.initialize().await.is_err());
        session.close().await.unwrap();
    });
    assert_eq!(mock.requests.borrow().len(), 3);
}

#[test]
fn total_timeout_includes_open_response_body_and_does_not_replay() {
    let mut streamed = response(200, "text/event-stream", b": progress\n\n".to_vec());
    streamed.body.stay_open = true;
    let (mock, mut session) = fixture_with_timeout(vec![initialize(), ack(), streamed], 20);
    run(async {
        session.initialize().await.unwrap();
        let error = session.request("tools/call", json!({})).await.unwrap_err();
        assert!(matches!(error, Error::Transport(text) if text.contains("timeout")));
        assert!(session.request("tools/call", json!({})).await.is_err());
    });
    assert_eq!(mock.requests.borrow().len(), 3);
}

#[test]
fn negotiation_and_capabilities_are_required() {
    for result in [
        json!({"protocolVersion":"future","capabilities":{"tools":{}}}),
        json!({"protocolVersion":"2025-11-25","capabilities":{}}),
        json!({"protocolVersion":"2025-11-25","capabilities":{"tools":true}}),
        json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{"listChanged":1}}}),
        json!({"protocolVersion":"2025-11-25","capabilities":{"tools":{}}}),
    ] {
        let (mock, mut session) = fixture(vec![rpc(1, result)]);
        assert!(matches!(run(session.initialize()), Err(Error::Protocol(_))));
        assert_eq!(mock.requests.borrow().len(), 1);
    }
}

#[test]
fn session_header_is_visible_ascii_unique_and_stable() {
    for value in ["", "two ids", "\n", "non-ascii-\u{e9}"] {
        let mut first = initialize();
        first.headers.retain(|(key, _)| key != "MCP-Session-Id");
        first.headers.push(("mcp-session-id".into(), value.into()));
        let (_, mut session) = fixture(vec![first]);
        assert!(matches!(run(session.initialize()), Err(Error::Protocol(_))));
    }
    let mut first = initialize();
    first
        .headers
        .push(("mcp-session-id".into(), "again".into()));
    let (_, mut session) = fixture(vec![first]);
    assert!(run(session.initialize()).is_err());

    let mut reply = rpc(2, json!({}));
    reply
        .headers
        .push(("mcp-session-id".into(), "changed".into()));
    let (_, mut session) = fixture(vec![initialize(), ack(), reply]);
    run(async {
        session.initialize().await.unwrap();
        assert!(session.request("tools/list", json!({})).await.is_err());
    });
}

#[test]
fn initialized_ack_must_be_empty_202() {
    for reply in [response(200, "", vec![]), response(202, "", b"x".to_vec())] {
        let (_, mut session) = fixture(vec![initialize(), reply]);
        assert!(matches!(run(session.initialize()), Err(Error::Protocol(_))));
    }
}

#[test]
fn transport_statuses_do_not_expose_response_bodies_or_follow_redirects() {
    for status in [301, 302, 307, 401, 403, 500] {
        let (mock, mut session) = fixture(vec![response(
            status,
            "application/json",
            b"fixture-token and sensitive server diagnostic".to_vec(),
        )]);
        let error = run(session.initialize()).unwrap_err();
        assert!(matches!(error, Error::Transport(_)));
        assert!(!error.to_string().contains("fixture-token"));
        assert_eq!(mock.requests.borrow().len(), 1);
    }
}

#[test]
fn rpc_error_payload_preserved_with_credentials_redacted() {
    let error = response(
        200,
        "application/json",
        br#"{"jsonrpc":"2.0","id":2,"error":{"code":-32602,"message":"fixture-token private-session","data":{"field":"name"}}}"#.to_vec(),
    );
    let (_, mut session) = fixture(vec![initialize(), ack(), error]);
    run(async {
        session.initialize().await.unwrap();
        let error = session.request("tools/call", json!({})).await.unwrap_err();
        let text = error.to_string();
        assert!(matches!(error, Error::Protocol(_)));
        assert!(text.contains("-32602") && text.contains("field"));
        assert!(!text.contains("fixture-token") && !text.contains("private-session"));
    });
}

#[test]
fn matching_ids_content_type_and_complete_sse_are_required() {
    for response in [
        rpc(9, json!({})),
        response(200, "text/html", b"html".to_vec()),
        response(
            200,
            "text/event-stream",
            b"data: {\"jsonrpc\":\"2.0\",\"id\":2,\"result\":{}}\n".to_vec(),
        ),
        response(200, "text/event-stream", b": finished\n\n".to_vec()),
        response(202, "", vec![]),
    ] {
        let (_, mut session) = fixture(vec![initialize(), ack(), response]);
        run(async {
            session.initialize().await.unwrap();
            assert!(matches!(
                session.request("tools/list", json!({})).await,
                Err(Error::Protocol(_))
            ));
        });
    }
}

#[test]
fn body_and_event_limits_are_enforced() {
    for content_type in ["application/json", "text/event-stream"] {
        let mock = Mock {
            responses: Rc::new(RefCell::new(
                [response(200, content_type, vec![b' '; 257])].into(),
            )),
            requests: Rc::new(RefCell::new(vec![])),
        };
        let mut session = Session::new(
            mock,
            "http://localhost/mcp".into(),
            None,
            Limits {
                max_response_bytes: 256,
                max_event_bytes: 128,
                ..Limits::default()
            },
        )
        .unwrap();
        assert!(
            run(session.initialize())
                .unwrap_err()
                .to_string()
                .contains("limit")
        );
    }
}

#[test]
fn tool_errors_precede_schema_success_and_keep_envelopes() {
    let result = json!({"isError":true,"content":[{"type":"text","text":"failed"}],"structuredContent":{"wrong":true},"_meta":{"trace":"a"}});
    assert_eq!(
        tool_result(result.clone()),
        Err(Error::ToolExecution(result.to_string()))
    );
    let success = json!({"content":[],"structuredContent":{"answer":42},"_meta":{}});
    assert_eq!(tool_result(success.clone()).unwrap(), success);
    for invalid in [
        json!({}),
        json!({"content":[],"isError":"false"}),
        json!({"content":[],"structuredContent":null}),
    ] {
        assert!(matches!(tool_result(invalid), Err(Error::Protocol(_))));
    }
}

#[test]
fn invalid_endpoint_bearer_or_limits_fail_before_io() {
    let (mock, _) = fixture(vec![]);
    for endpoint in [
        "file:///secret",
        "/mcp",
        "http://user:password@localhost/mcp",
        "http://localhost/mcp#fragment",
    ] {
        assert!(Session::new(mock.clone(), endpoint.into(), None, Limits::default()).is_err());
    }
    for bearer in ["", "line\nbreak", "space token"] {
        assert!(
            Session::new(
                mock.clone(),
                "http://localhost/mcp".into(),
                Some(bearer.into()),
                Limits::default()
            )
            .is_err()
        );
    }
    assert!(
        Session::new(
            mock.clone(),
            "http://localhost/mcp".into(),
            None,
            Limits {
                request_timeout_ms: 0,
                ..Limits::default()
            }
        )
        .is_err()
    );
    assert!(mock.requests.borrow().is_empty());
}

#[test]
fn wasi_timeout_representations_cannot_overflow() {
    let maximum = u64::MAX / 1_000_000;
    assert!(
        Limits {
            request_timeout_ms: maximum,
            ..Limits::default()
        }
        .validate()
        .is_ok()
    );
    assert!(
        Limits {
            request_timeout_ms: maximum + 1,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
}

#[test]
fn server_ping_during_initialize_gets_session_header_before_negotiation() {
    let mut first = initialize();
    first.headers.retain(|(name, _)| name != "Content-Type");
    first
        .headers
        .push(("content-type".into(), "text/event-stream".into()));
    first.body.chunks = [Ok(format!(
        "data: {{\"jsonrpc\":\"2.0\",\"id\":99,\"method\":\"ping\"}}\n\n\
         data: {}\n\n",
        json!({"jsonrpc":"2.0","id":1,"result":initialized_result()})
    )
    .into_bytes())]
    .into();
    first.body.stay_open = true;
    let (mock, mut session) = fixture(vec![first, ack(), ack()]);
    run(session.initialize()).unwrap();
    let requests = mock.requests.borrow();
    assert_eq!(requests.len(), 3);
    let reply = requests.get(1).unwrap();
    assert!(
        reply
            .headers
            .contains(&("mcp-session-id".into(), "private-session".into()))
    );
    assert!(
        !reply
            .headers
            .iter()
            .any(|(name, _)| name == "mcp-protocol-version")
    );
    let value: Value = serde_json::from_slice(&reply.body).unwrap();
    assert_eq!(value.get("id"), Some(&json!(99)));
    assert_eq!(value.get("result"), Some(&json!({})));
}

#[test]
fn stateless_server_needs_no_delete_but_still_negotiates_protocol() {
    let (mock, mut session) = fixture(vec![rpc(1, initialized_result()), ack(), rpc(2, json!({}))]);
    run(async {
        session.initialize().await.unwrap();
        session.request("tools/list", json!({})).await.unwrap();
        session.close().await.unwrap();
    });
    let requests = mock.requests.borrow();
    assert_eq!(requests.len(), 3);
    for request in requests.iter().skip(1) {
        assert!(
            !request
                .headers
                .iter()
                .any(|(name, _)| name == "mcp-session-id")
        );
        assert!(
            request
                .headers
                .contains(&("mcp-protocol-version".into(), "2025-11-25".into()))
        );
    }
}
