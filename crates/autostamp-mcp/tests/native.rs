//! Real loopback streaming HTTP transport and deadline regressions.
#![cfg(all(feature = "cli", not(target_arch = "wasm32")))]

use std::{
    cell::Cell,
    future::{Future, poll_fn},
    pin::pin,
    rc::Rc,
    task::{Context, Poll, Waker},
    time::{Duration, Instant},
};

use autostamp_mcp::{
    native::NativeTransport,
    runtime::{Body, Error, Limits, PROTOCOL_VERSION, Request, Session, Transport},
};
use serde_json::{Value, json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
    time::{sleep, timeout},
};

const TOKEN: &str = "test_Q8vN5xP2zW9bL4jH7dF3rS6kA1uE0cY5nM8tB2gR9wK4pV7";
const SOCKET_DEADLINE: Duration = Duration::from_secs(3);
const SESSION_ID: &str = "local-fixture-session";

struct Server {
    endpoint: String,
    task: Option<JoinHandle<()>>,
}

impl Server {
    async fn start<F, Fut>(serve: F) -> Self
    where
        F: FnOnce(TcpListener) -> Fut + Send + 'static,
        Fut: Future<Output = ()> + Send + 'static,
    {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let endpoint = format!("http://{}/mcp", listener.local_addr().unwrap());
        let task = tokio::spawn(async move {
            timeout(Duration::from_secs(5), serve(listener))
                .await
                .expect("mock server exceeded its total deadline");
        });
        Self {
            endpoint,
            task: Some(task),
        }
    }

    async fn finish(mut self) {
        self.task
            .as_mut()
            .unwrap()
            .await
            .expect("mock server failed");
        self.task.take();
    }
}

impl Drop for Server {
    fn drop(&mut self) {
        // Includes assertion failures: no detached server or listener survives.
        if let Some(task) = &self.task {
            task.abort();
        }
    }
}

#[derive(Debug)]
struct Received {
    method: String,
    target: String,
    headers: Vec<(String, String)>,
    body: Vec<u8>,
}

impl Received {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_slice(&self.body).unwrap()
    }
}

async fn receive(listener: &TcpListener) -> (TcpStream, Received) {
    timeout(SOCKET_DEADLINE, async {
        let (mut socket, _) = listener.accept().await.unwrap();
        let mut head = Vec::new();
        while !head.ends_with(b"\r\n\r\n") {
            assert!(head.len() < 64 * 1024, "oversized request headers");
            head.push(socket.read_u8().await.unwrap());
        }
        let head = String::from_utf8(head).unwrap();
        let mut lines = head.split("\r\n");
        let mut start = lines.next().unwrap().split_whitespace();
        let method = start.next().unwrap().to_owned();
        let target = start.next().unwrap().to_owned();
        assert_eq!(start.next(), Some("HTTP/1.1"));
        let headers: Vec<_> = lines
            .filter(|line| !line.is_empty())
            .map(|line| {
                let (name, value) = line.split_once(':').unwrap();
                (name.to_owned(), value.trim().to_owned())
            })
            .collect();
        let size = headers
            .iter()
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .map(|(_, value)| value.parse::<usize>().unwrap())
            .unwrap_or(0);
        assert!(size < 64 * 1024);
        let mut body = vec![0; size];
        socket.read_exact(&mut body).await.unwrap();
        (
            socket,
            Received {
                method,
                target,
                headers,
                body,
            },
        )
    })
    .await
    .expect("accept/request read deadline")
}

async fn write(socket: &mut TcpStream, bytes: &[u8]) {
    timeout(SOCKET_DEADLINE, socket.write_all(bytes))
        .await
        .expect("socket write deadline")
        .expect("socket write failed");
}

async fn response(socket: &mut TcpStream, status: u16, headers: &str, body: &[u8]) {
    let head = format!(
        "HTTP/1.1 {status} Fixture\r\nContent-Length: {}\r\nConnection: close\r\n{headers}\r\n",
        body.len()
    );
    write(socket, head.as_bytes()).await;
    write(socket, body).await;
}

async fn json_response(socket: &mut TcpStream, id: Value, result: Value, session: bool) {
    let headers = if session {
        "Content-Type: application/json\r\nMcP-SeSsIoN-Id: local-fixture-session\r\n"
    } else {
        "Content-Type: application/json\r\n"
    };
    response(
        socket,
        200,
        headers,
        json!({"jsonrpc":"2.0","id":id,"result":result})
            .to_string()
            .as_bytes(),
    )
    .await;
}

fn init_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": {"tools":{"listChanged":false}},
        "serverInfo": {"name":"native-fixture","version":"1"}
    })
}

fn check_request(request: &Received, active: bool) {
    assert_eq!(request.method, "POST");
    assert_eq!(request.target, "/mcp");
    assert_eq!(
        request.header("authorization"),
        Some(format!("Bearer {TOKEN}").as_str())
    );
    assert_eq!(request.header("content-type"), Some("application/json"));
    assert_eq!(
        request.header("accept"),
        Some("application/json, text/event-stream")
    );
    assert_eq!(
        request.header("mcp-session-id"),
        active.then_some(SESSION_ID)
    );
    assert_eq!(
        request.header("mcp-protocol-version"),
        active.then_some(PROTOCOL_VERSION)
    );
}

async fn handshake(listener: &TcpListener) {
    let (mut socket, request) = receive(listener).await;
    check_request(&request, false);
    let message = request.json();
    assert_eq!(message.get("method").unwrap(), "initialize");
    assert_eq!(
        message.pointer("/params/protocolVersion").unwrap(),
        PROTOCOL_VERSION
    );
    json_response(
        &mut socket,
        message.get("id").unwrap().clone(),
        init_result(),
        true,
    )
    .await;
    drop(socket);

    let (mut socket, request) = receive(listener).await;
    check_request(&request, true);
    let message = request.json();
    assert_eq!(message.get("method").unwrap(), "notifications/initialized");
    assert!(message.get("id").is_none());
    response(&mut socket, 202, "", b"").await;
}

fn session(endpoint: &str, timeout_ms: u64) -> Session<NativeTransport> {
    Session::new(
        NativeTransport::new(37).unwrap(),
        endpoint.to_owned(),
        Some(TOKEN.into()),
        Limits {
            request_timeout_ms: timeout_ms,
            ..Limits::default()
        },
    )
    .unwrap()
}

fn request(endpoint: &str) -> Request {
    Request {
        method: "POST".into(),
        endpoint: endpoint.into(),
        headers: vec![("Authorization".into(), format!("Bearer {TOKEN}"))],
        body: b"one-request-body".to_vec(),
    }
}

fn assert_safe(value: impl std::fmt::Debug) {
    assert!(!format!("{value:?}").contains(TOKEN));
}

fn assert_timeout(error: &Error) {
    assert!(
        matches!(error, Error::Transport(message) if message.contains("total request timeout"))
    );
    assert_safe(error);
}

async fn peer_closed(socket: &mut TcpStream) {
    let mut byte = [0];
    match timeout(SOCKET_DEADLINE, socket.read(&mut byte)).await {
        Ok(Ok(0)) => {}
        Ok(Err(error))
            if matches!(
                error.kind(),
                std::io::ErrorKind::ConnectionReset | std::io::ErrorKind::BrokenPipe
            ) => {}
        result => panic!("cancelled request did not close its socket: {result:?}"),
    }
}

async fn sse_start(socket: &mut TcpStream) {
    write(
        socket,
        b"HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nTransfer-Encoding: chunked\r\n\r\n",
    )
    .await;
}

async fn chunk(socket: &mut TcpStream, bytes: &[u8]) {
    write(socket, format!("{:x}\r\n", bytes.len()).as_bytes()).await;
    write(socket, bytes).await;
    write(socket, b"\r\n").await;
}

#[tokio::test]
async fn session_initializes_notifies_calls_and_deletes_with_correct_headers() {
    let server = Server::start(|listener| async move {
        handshake(&listener).await;
        let (mut socket, request) = receive(&listener).await;
        check_request(&request, true);
        let message = request.json();
        assert_eq!(message.get("method").unwrap(), "tools/call");
        assert_eq!(
            *message.get("params").unwrap(),
            json!({"name":"echo","arguments":{"x":3}})
        );
        json_response(
            &mut socket,
            message.get("id").unwrap().clone(),
            json!({"content":[],"structuredContent":{"x":3}}),
            false,
        )
        .await;
        drop(socket);
        let (mut socket, request) = receive(&listener).await;
        assert_eq!(request.method, "DELETE");
        assert_eq!(request.header("mcp-session-id"), Some(SESSION_ID));
        assert_eq!(
            request.header("mcp-protocol-version"),
            Some(PROTOCOL_VERSION)
        );
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        assert!(request.body.is_empty());
        response(&mut socket, 204, "", b"").await;
    })
    .await;
    let mut session = session(&server.endpoint, 2_000);
    assert_safe(&session);
    session.initialize().await.unwrap();
    let result = session
        .request("tools/call", json!({"name":"echo","arguments":{"x":3}}))
        .await
        .unwrap();
    assert_eq!(result, json!({"content":[],"structuredContent":{"x":3}}));
    session.close().await.unwrap();
    session.close().await.unwrap();
    server.finish().await;
}

#[tokio::test]
async fn preserves_duplicate_headers_status_and_bounds_streamed_copies() {
    let server = Server::start(|listener| async move {
        let (mut socket, request) = receive(&listener).await;
        assert_eq!(request.body, b"one-request-body");
        let values: Vec<_> = request
            .headers
            .iter()
            .filter(|(name, _)| name.eq_ignore_ascii_case("x-repeat"))
            .map(|(_, value)| value.as_str())
            .collect();
        assert_eq!(values, ["first", "second"]);
        response(
            &mut socket,
            503,
            "X-Repeat: first\r\nx-repeat: second\r\nRetry-After: 0\r\n",
            &vec![b'x'; 32_769],
        )
        .await;
        assert!(
            timeout(Duration::from_millis(100), listener.accept())
                .await
                .is_err(),
            "unexpected retry"
        );
    })
    .await;
    let transport = NativeTransport::new(7).unwrap();
    let mut request = request(&server.endpoint);
    request.headers.extend([
        ("X-Repeat".into(), "first".into()),
        ("x-repeat".into(), "second".into()),
    ]);
    assert_safe(&request);
    assert_safe(&transport);
    transport
        .within(Duration::from_secs(2), async {
            let mut response = transport.send(request).await?;
            assert_eq!(response.status, 503);
            assert_eq!(
                response
                    .headers
                    .iter()
                    .filter(|(key, _)| key.eq_ignore_ascii_case("x-repeat"))
                    .map(|(_, value)| value.as_str())
                    .collect::<Vec<_>>(),
                ["first", "second"]
            );
            assert_safe(&response);
            assert_safe(&response.body);
            let mut count = 0;
            while let Some(chunk) = response.body.next().await? {
                assert!(chunk.len() <= 7);
                assert!(chunk.iter().all(|byte| *byte == b'x'));
                count += chunk.len();
            }
            assert_eq!(count, 32_769);
            assert!(response.body.next().await?.is_none());
            Ok(())
        })
        .await
        .unwrap();
    server.finish().await;
}

#[tokio::test]
async fn session_rejects_repeated_protocol_headers_without_collapsing_them() {
    for headers in [
        "Content-Type: application/json\r\ncontent-type: application/json\r\n",
        "Content-Type: application/json\r\nMcp-Session-Id: first\r\nmcp-session-id: second\r\n",
    ] {
        let server = Server::start(move |listener| async move {
            let (mut socket, request) = receive(&listener).await;
            let body = json!({"jsonrpc":"2.0","id":request.json().get("id").unwrap(),"result":init_result()});
            response(&mut socket, 200, headers, body.to_string().as_bytes()).await;
        })
        .await;
        let error = session(&server.endpoint, 2_000)
            .initialize()
            .await
            .unwrap_err();
        assert!(matches!(error, Error::Protocol(ref text) if text.contains("repeated")));
        server.finish().await;
    }
}

#[tokio::test]
async fn incremental_sse_handles_ping_and_returns_before_eof() {
    let server = Server::start(|listener| async move {
        handshake(&listener).await;
        let (mut stream, request) = receive(&listener).await;
        check_request(&request, true);
        let id = request.json().get("id").unwrap().clone();
        sse_start(&mut stream).await;
        chunk(&mut stream, b": keepalive\n\ndata: {\"jsonrpc\":\"2.0\",").await;
        sleep(Duration::from_millis(10)).await;
        chunk(
            &mut stream,
            b"\"id\":\"server-ping\",\"method\":\"ping\"}\n\n",
        )
        .await;

        let (mut reply_socket, reply) = receive(&listener).await;
        check_request(&reply, true);
        assert_eq!(
            reply.json(),
            json!({"jsonrpc":"2.0","id":"server-ping","result":{}})
        );
        response(&mut reply_socket, 202, "", b"").await;
        let result = format!(
            "data: {}\n\ndata: unfinished",
            json!({"jsonrpc":"2.0","id":id,"result":{"tools":[]}})
        );
        for part in result.as_bytes().chunks(11) {
            chunk(&mut stream, part).await;
        }
        // No zero chunk/EOF: only the matching RPC reply can complete the call.
        peer_closed(&mut stream).await;
    })
    .await;
    let mut session = session(&server.endpoint, 2_000);
    session.initialize().await.unwrap();
    let result = session.request("tools/list", json!({})).await.unwrap();
    assert_eq!(result, json!({"tools":[]}));
    server.finish().await;
}

#[tokio::test]
async fn timeout_before_headers_cancels_the_connection() {
    let server = Server::start(|listener| async move {
        let (mut socket, _) = receive(&listener).await;
        peer_closed(&mut socket).await;
    })
    .await;
    let error = session(&server.endpoint, 100)
        .initialize()
        .await
        .unwrap_err();
    assert_timeout(&error);
    server.finish().await;
}

#[tokio::test]
async fn timeout_spans_headers_and_body_progress_without_resetting() {
    let server = Server::start(|listener| async move {
        let (mut socket, _) = receive(&listener).await;
        sleep(Duration::from_millis(80)).await;
        write(
            &mut socket,
            b"HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nTransfer-Encoding: chunked\r\n\r\n",
        )
        .await;
        chunk(&mut socket, b"{\"jsonrpc\":").await;
        sleep(Duration::from_millis(100)).await;
        chunk(&mut socket, b"\"2.0\",").await;
        let since_last_chunk = Instant::now();
        peer_closed(&mut socket).await;
        assert!(
            since_last_chunk.elapsed() < Duration::from_millis(200),
            "body progress reset the total deadline"
        );
    })
    .await;
    let started = Instant::now();
    let error = session(&server.endpoint, 250)
        .initialize()
        .await
        .unwrap_err();
    assert_timeout(&error);
    assert!(started.elapsed() >= Duration::from_millis(240));
    assert!(started.elapsed() < Duration::from_millis(400));
    server.finish().await;
}

#[tokio::test]
async fn nested_ping_reply_shares_the_streams_total_deadline() {
    let server = Server::start(|listener| async move {
        handshake(&listener).await;
        let (mut stream, _) = receive(&listener).await;
        sse_start(&mut stream).await;
        sleep(Duration::from_millis(150)).await;
        chunk(
            &mut stream,
            b"data: {\"jsonrpc\":\"2.0\",\"id\":\"late-ping\",\"method\":\"ping\"}\n\n",
        )
        .await;
        let (mut reply, request) = receive(&listener).await;
        assert_eq!(request.json().get("id").unwrap(), "late-ping");
        let started = Instant::now();
        peer_closed(&mut reply).await;
        assert!(started.elapsed() < Duration::from_millis(200));
        peer_closed(&mut stream).await;
    })
    .await;
    let mut session = session(&server.endpoint, 250);
    session.initialize().await.unwrap();
    let error = session.request("tools/list", json!({})).await.unwrap_err();
    assert_timeout(&error);
    server.finish().await;
}

#[tokio::test]
async fn redirects_are_returned_and_never_forward_credentials() {
    let destination = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let location = format!("http://{}/{TOKEN}", destination.local_addr().unwrap());
    let server = Server::start(move |listener| async move {
        let (mut socket, request) = receive(&listener).await;
        assert_eq!(request.method, "POST");
        assert_eq!(
            request.header("authorization"),
            Some(format!("Bearer {TOKEN}").as_str())
        );
        response(&mut socket, 307, &format!("Location: {location}\r\n"), b"").await;
    })
    .await;
    let transport = NativeTransport::new(64).unwrap();
    let response = transport
        .within(
            Duration::from_secs(2),
            transport.send(request(&server.endpoint)),
        )
        .await
        .unwrap();
    assert_eq!(response.status, 307);
    assert_safe(&response);
    assert!(
        timeout(Duration::from_millis(100), destination.accept())
            .await
            .is_err(),
        "redirect destination received a connection"
    );
    server.finish().await;
}

#[tokio::test]
async fn a_closed_connection_never_replays_the_post() {
    let server = Server::start(|listener| async move {
        let (socket, request) = receive(&listener).await;
        assert_eq!(request.method, "POST");
        assert_eq!(request.body, b"one-request-body");
        drop(socket);
        assert!(
            timeout(Duration::from_millis(150), listener.accept())
                .await
                .is_err(),
            "request was replayed"
        );
    })
    .await;
    let transport = NativeTransport::new(64).unwrap();
    let endpoint = format!("{}?credential={TOKEN}", server.endpoint);
    let error = transport
        .within(Duration::from_secs(2), transport.send(request(&endpoint)))
        .await
        .unwrap_err();
    assert_safe(&error);
    assert!(!error.to_string().contains(&endpoint));
    assert!(matches!(error, Error::Transport(_)));
    server.finish().await;
}

#[tokio::test]
async fn validates_outgoing_methods_headers_and_endpoints_without_leaking() {
    assert!(NativeTransport::new(0).is_err());
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let endpoint = format!("http://{}/", listener.local_addr().unwrap());
    let transport = NativeTransport::new(64).unwrap();
    for case in 0..5 {
        let mut request = request(&endpoint);
        let expected = match case {
            0 => {
                request.headers.push((format!("{TOKEN}\n"), "value".into()));
                "header name"
            }
            1 => {
                request
                    .headers
                    .push(("authorization".into(), format!("{TOKEN}\r\nbad")));
                "header value"
            }
            2 => {
                request.method = format!("{TOKEN}\n");
                "HTTP method"
            }
            3 => {
                request.endpoint = format!("http://{TOKEN}@127.0.0.1/");
                "userinfo"
            }
            _ => {
                request.endpoint = format!("{endpoint}#{TOKEN}");
                "fragment"
            }
        };
        let error = transport
            .within(Duration::from_secs(2), transport.send(request))
            .await
            .unwrap_err();
        assert!(error.to_string().contains(expected));
        assert_safe(&error);
        assert!(!error.to_string().contains(TOKEN));
    }
    assert!(
        timeout(Duration::from_millis(50), listener.accept())
            .await
            .is_err()
    );
}

#[tokio::test]
async fn malformed_http_and_non_ascii_response_headers_are_sanitized() {
    for case in 0..3 {
        let server = Server::start(move |listener| async move {
            let (mut socket, _) = receive(&listener).await;
            let bytes = match case {
                0 => format!("HTTP/1.1 {TOKEN}\r\n\r\n").into_bytes(),
                1 => [
                    format!("HTTP/1.1 200 OK\r\nX-Secret: {TOKEN}").as_bytes(),
                    b"\xff\r\nContent-Length: 0\r\n\r\n",
                ]
                .concat(),
                _ => format!("HTTP/1.1 200 OK\r\n{TOKEN}\x01: invalid\r\n\r\n").into_bytes(),
            };
            write(&mut socket, &bytes).await;
        })
        .await;
        let transport = NativeTransport::new(64).unwrap();
        let endpoint = format!("{}?token={TOKEN}", server.endpoint);
        let error = transport
            .within(Duration::from_secs(2), transport.send(request(&endpoint)))
            .await
            .unwrap_err();
        assert_safe(&error);
        assert!(!error.to_string().contains(TOKEN));
        assert!(!error.to_string().contains(&endpoint));
        if case == 1 {
            assert_eq!(
                error,
                Error::Protocol("non-ASCII HTTP response header".into())
            );
        } else {
            assert!(matches!(error, Error::Transport(ref text) if text.contains("malformed")));
        }
        server.finish().await;
    }
}

#[tokio::test]
async fn truncated_body_is_a_sanitized_transport_error() {
    let server = Server::start(|listener| async move {
        let (mut socket, _) = receive(&listener).await;
        write(
            &mut socket,
            format!("HTTP/1.1 200 OK\r\nContent-Length: 1000\r\n\r\n{TOKEN}").as_bytes(),
        )
        .await;
    })
    .await;
    let transport = NativeTransport::new(8).unwrap();
    let error = transport
        .within(Duration::from_secs(2), async {
            let mut response = transport.send(request(&server.endpoint)).await?;
            assert_safe(&response.body);
            while response.body.next().await?.is_some() {}
            Ok(())
        })
        .await
        .unwrap_err();
    assert!(matches!(error, Error::Transport(ref text) if text.contains("body")));
    assert_safe(&error);
    server.finish().await;
}

struct DropFlag(Rc<Cell<bool>>);

impl Drop for DropFlag {
    fn drop(&mut self) {
        self.0.set(true);
    }
}

#[tokio::test]
async fn absolute_deadline_wins_without_timer_driver_progress_and_drops_future() {
    let transport = NativeTransport::new(64).unwrap();
    let dropped = Rc::new(Cell::new(false));
    let polls = Rc::new(Cell::new(0_u64));
    let guard = DropFlag(dropped.clone());
    let count = polls.clone();
    let pending = async move {
        let _guard = guard;
        poll_fn(move |cx| {
            count.set(count.get() + 1);
            cx.waker().wake_by_ref();
            Poll::<Result<(), Error>>::Pending
        })
        .await
    };
    let mut cx = Context::from_waker(Waker::noop());
    let started = Instant::now();
    {
        let mut operation = pin!(transport.within(Duration::from_millis(30), pending));
        loop {
            // Deliberately never yield to Tokio's timer driver.
            if let Poll::Ready(result) = operation.as_mut().poll(&mut cx) {
                assert_timeout(&result.unwrap_err());
                break;
            }
            assert!(started.elapsed() < Duration::from_millis(500));
        }
    }
    assert!(polls.get() > 1);
    assert!(dropped.get());
    assert!(started.elapsed() >= Duration::from_millis(30));
    assert!(started.elapsed() < Duration::from_millis(500));
}

#[tokio::test(start_paused = true)]
async fn timer_is_checked_before_a_ready_future_at_the_deadline() {
    let transport = NativeTransport::new(64).unwrap();
    let polled = Cell::new(false);
    let result = transport
        .within(Duration::ZERO, async {
            polled.set(true);
            Ok(())
        })
        .await;
    assert_timeout(&result.unwrap_err());
    assert!(!polled.get(), "expired deadline polled the request first");
}

#[tokio::test]
async fn deadline_wins_if_one_poll_finishes_late() {
    let transport = NativeTransport::new(64).unwrap();
    let error = transport
        .within(Duration::from_millis(10), async {
            let started = Instant::now();
            while started.elapsed() < Duration::from_millis(20) {
                std::hint::spin_loop();
            }
            Ok(())
        })
        .await
        .unwrap_err();
    assert_timeout(&error);
}

#[tokio::test]
async fn request_body_and_headers_debug_never_disclose_credentials() {
    let transport = NativeTransport::new(64).unwrap();
    let mut request = request(&format!("http://127.0.0.1/?token={TOKEN}"));
    request.body = TOKEN.as_bytes().to_vec();
    assert_safe(&request);
    assert_safe(transport.clone());
    let invalid_bearer = format!("{TOKEN}\n");
    let error = Session::new(
        transport,
        "http://127.0.0.1/".into(),
        Some(invalid_bearer),
        Limits::default(),
    )
    .unwrap_err();
    assert_safe(&error);
    assert!(!error.to_string().contains(TOKEN));
}
