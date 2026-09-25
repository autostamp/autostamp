//! Transport-independent, bounded MCP Streamable HTTP sessions.
//!
//! Adapters must not follow redirects, retry requests, or include credentials in
//! diagnostics. `Transport::within` must cancel the supplied future at its total
//! deadline, including while reading a body or replying to a server request.
#![allow(async_fn_in_trait)]

use std::{
    fmt,
    future::{Future, poll_fn},
    task::Poll,
    time::Duration,
};

use serde_json::{Value, json};

pub mod wire;

use wire::{ResponseMatcher, RpcId, RpcMessage, SseDecoder, SseLimits};

/// Protocol version implemented by this runtime.
pub const PROTOCOL_VERSION: &str = "2025-11-25";

/// Resource limits, applied independently to each HTTP exchange.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Limits {
    /// Total deadline, not an idle timeout; progress does not reset it.
    pub request_timeout_ms: u64,
    /// Maximum response bytes consumed before the matching reply.
    pub max_response_bytes: usize,
    /// Maximum SSE event and physical line bytes.
    pub max_event_bytes: usize,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            request_timeout_ms: 30_000,
            max_response_bytes: 4 * 1024 * 1024,
            max_event_bytes: 1024 * 1024,
        }
    }
}

impl Limits {
    /// Reject unusable limits before performing any I/O.
    pub fn validate(self) -> Result<(), Error> {
        if self.request_timeout_ms == 0
            || self.request_timeout_ms > u64::MAX / 1_000_000
            || self.max_response_bytes == 0
            || self.max_event_bytes == 0
            || self.max_event_bytes > self.max_response_bytes
        {
            return Err(Error::Protocol(
                "limits must be positive, timeout must fit WASI nanoseconds, and event bytes must not exceed response bytes".into(),
            ));
        }
        Ok(())
    }
}

/// Errors correspond directly to the generated WIT error variant.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Error {
    /// HTTP, I/O, credentials, or total-deadline failure.
    Transport(String),
    /// Framing, JSON-RPC, negotiation, or session failure.
    Protocol(String),
    /// Full CallToolResult envelope when `isError` is true.
    ToolExecution(String),
    /// A success result or arguments violate the declared structural schema.
    ResultDecoding(String),
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let (class, message) = match self {
            Self::Transport(message) => ("transport", message),
            Self::Protocol(message) => ("protocol", message),
            Self::ToolExecution(message) => ("tool execution", message),
            Self::ResultDecoding(message) => ("result decoding", message),
        };
        write!(f, "{class}: {message}")
    }
}

impl std::error::Error for Error {}

impl From<wire::WireError> for Error {
    fn from(value: wire::WireError) -> Self {
        Self::Protocol(value.to_string())
    }
}

/// An HTTP request. Debug output deliberately omits the endpoint and headers.
pub struct Request {
    /// POST or DELETE.
    pub method: String,
    /// Original configured HTTP endpoint; never redirect it.
    pub endpoint: String,
    /// Header names and values, including potentially sensitive authorization.
    pub headers: Vec<(String, String)>,
    /// One serialized JSON-RPC message, or empty for DELETE.
    pub body: Vec<u8>,
}

impl fmt::Debug for Request {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Request")
            .field("method", &self.method)
            .field("body_bytes", &self.body.len())
            .finish_non_exhaustive()
    }
}

/// HTTP response metadata with a streaming body.
pub struct Response<B> {
    /// HTTP status code.
    pub status: u16,
    /// Preserve repeated headers so the session can reject ambiguous values.
    pub headers: Vec<(String, String)>,
    /// Streaming response body.
    pub body: B,
}

impl<B> fmt::Debug for Response<B> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Response")
            .field("status", &self.status)
            .finish_non_exhaustive()
    }
}

/// An incremental body; dropping it must cancel/release the HTTP stream.
pub trait Body {
    /// Return the next bounded transport chunk, or `None` on actual EOF.
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error>;
}

/// Thread-local asynchronous HTTP adapter, shared by native and WASI clients.
pub trait Transport {
    /// The adapter's streaming body.
    type Body: Body;

    /// Send exactly once, without redirects or automatic retries.
    async fn send(&self, request: Request) -> Result<Response<Self::Body>, Error>;

    /// Enforce a total wall-clock deadline and drop the future on timeout.
    ///
    /// The future includes all reads and nested POST replies. Implementations
    /// must poll the timer even when body reads remain continuously ready.
    async fn within<T>(
        &self,
        timeout: Duration,
        future: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error>;
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum State {
    New,
    Active,
    Failed,
    Closed,
}

struct Context {
    endpoint: String,
    bearer: Option<String>,
    session: Option<String>,
    version: Option<String>,
    limits: Limits,
}

/// One explicitly initialized session. Failed sessions cannot be reused.
///
/// Each generated tool export constructs a fresh session. Native discovery can
/// reuse an active session for pagination, then explicitly call `close`.
pub struct Session<T> {
    transport: T,
    context: Context,
    state: State,
    next_id: u64,
}

impl<T> fmt::Debug for Session<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Session")
            .field("state", &self.state)
            .field("limits", &self.context.limits)
            .finish_non_exhaustive()
    }
}

/// Validate a credential-free absolute HTTP(S) endpoint without printing it.
pub fn validate_endpoint(endpoint: &str) -> Result<(), Error> {
    let uri: http::Uri = endpoint
        .parse()
        .map_err(|_| Error::Protocol("endpoint must be an absolute HTTP(S) URI".into()))?;
    if !matches!(uri.scheme_str(), Some("http" | "https"))
        || uri.host().is_none_or(str::is_empty)
        || uri.authority().is_some_and(|a| a.as_str().contains('@'))
        || endpoint.contains('#')
    {
        return Err(Error::Protocol(
            "endpoint must be HTTP(S), with a host and without userinfo or fragment".into(),
        ));
    }
    Ok(())
}

impl<T: Transport> Session<T> {
    /// Create an uninitialized session; the bearer value must come from a secret.
    pub fn new(
        transport: T,
        endpoint: String,
        bearer: Option<String>,
        limits: Limits,
    ) -> Result<Self, Error> {
        validate_endpoint(&endpoint)?;
        limits.validate()?;
        if bearer.as_ref().is_some_and(|value| {
            value.is_empty() || !value.bytes().all(|byte| matches!(byte, 0x21..=0x7e))
        }) {
            return Err(Error::Transport("invalid bearer credential".into()));
        }
        Ok(Self {
            transport,
            context: Context {
                endpoint,
                bearer,
                session: None,
                version: None,
                limits,
            },
            state: State::New,
            next_id: 1,
        })
    }

    /// Negotiate version/tools, then send `notifications/initialized`.
    pub async fn initialize(&mut self) -> Result<(), Error> {
        if self.state != State::New {
            return Err(Error::Protocol(
                "session cannot be initialized again".into(),
            ));
        }
        self.state = State::Failed;
        let result = self
            .exchange(
                "initialize",
                json!({
                    "protocolVersion": PROTOCOL_VERSION,
                    "capabilities": {},
                    "clientInfo": {"name":"autostamp-mcp","version":"0.1.0"}
                }),
                true,
            )
            .await?;
        if result.get("protocolVersion").and_then(Value::as_str) != Some(PROTOCOL_VERSION) {
            return Err(Error::Protocol(
                "unsupported negotiated protocol version".into(),
            ));
        }
        self.context.version = Some(PROTOCOL_VERSION.into());
        let capabilities = result.get("capabilities").and_then(Value::as_object);
        let tools = capabilities
            .and_then(|value| value.get("tools"))
            .and_then(Value::as_object)
            .ok_or_else(|| Error::Protocol("server must declare the tools capability".into()))?;
        if tools
            .get("listChanged")
            .is_some_and(|value| !value.is_boolean())
        {
            return Err(Error::Protocol(
                "tools.listChanged must be a boolean".into(),
            ));
        }
        let info = result.get("serverInfo").and_then(Value::as_object);
        if !["name", "version"].iter().all(|key| {
            info.and_then(|value| value.get(*key))
                .is_some_and(Value::is_string)
        }) {
            return Err(Error::Protocol(
                "serverInfo requires string name and version".into(),
            ));
        }
        let context = &mut self.context;
        let transport = &self.transport;
        transport
            .within(
                Duration::from_millis(context.limits.request_timeout_ms),
                acknowledge(
                    transport,
                    context,
                    json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
                ),
            )
            .await?;
        self.state = State::Active;
        Ok(())
    }

    /// Send a tools/list, tools/call, or ping request on this active session.
    ///
    /// A failed exchange poisons the session, preventing accidental replay.
    pub async fn request(&mut self, method: &str, params: Value) -> Result<Value, Error> {
        if self.state != State::Active {
            return Err(Error::Protocol(
                "session is not active; create a fresh session".into(),
            ));
        }
        if !matches!(method, "tools/list" | "tools/call" | "ping") || !params.is_object() {
            return Err(Error::Protocol(
                "unsupported method or non-object parameters".into(),
            ));
        }
        self.state = State::Failed;
        let result = self.exchange(method, params, false).await?;
        self.state = State::Active;
        Ok(result)
    }

    async fn exchange(
        &mut self,
        method: &str,
        params: Value,
        initializing: bool,
    ) -> Result<Value, Error> {
        let id = self.next_id;
        self.next_id = id
            .checked_add(1)
            .ok_or_else(|| Error::Protocol("request ID space exhausted".into()))?;
        let message = json!({"jsonrpc":"2.0","id":id,"method":method,"params":params});
        self.transport
            .within(
                Duration::from_millis(self.context.limits.request_timeout_ms),
                exchange(
                    &self.transport,
                    &mut self.context,
                    message,
                    RpcId::Number(id.into()),
                    initializing,
                ),
            )
            .await
    }

    /// Release a session exactly once. DELETE 405 is valid; expired IDs are gone.
    pub async fn close(&mut self) -> Result<(), Error> {
        self.state = State::Closed;
        if self.context.session.is_none() {
            return Ok(());
        }
        let request = self.context.http("DELETE", Vec::new());
        self.context.session = None;
        self.transport
            .within(
                Duration::from_millis(self.context.limits.request_timeout_ms),
                async {
                    let response = self.transport.send(request).await?;
                    if (200..300).contains(&response.status) || matches!(response.status, 404 | 405)
                    {
                        Ok(())
                    } else {
                        Err(http_error(response.status))
                    }
                },
            )
            .await
    }
}

impl Context {
    fn http(&self, method: &str, body: Vec<u8>) -> Request {
        let mut headers = vec![(
            "accept".into(),
            "application/json, text/event-stream".into(),
        )];
        if method == "POST" {
            headers.push(("content-type".into(), "application/json".into()));
        }
        if let Some(bearer) = &self.bearer {
            headers.push(("authorization".into(), format!("Bearer {bearer}")));
        }
        if let Some(session) = &self.session {
            headers.push(("mcp-session-id".into(), session.clone()));
        }
        if let Some(version) = &self.version {
            headers.push(("mcp-protocol-version".into(), version.clone()));
        }
        Request {
            method: method.into(),
            endpoint: self.endpoint.clone(),
            headers,
            body,
        }
    }

    fn status(&mut self, status: u16) -> Result<(), Error> {
        if status == 404 && self.session.take().is_some() {
            return Err(Error::Protocol(
                "MCP session expired; request was not replayed".into(),
            ));
        }
        if !(200..300).contains(&status) {
            return Err(http_error(status));
        }
        Ok(())
    }
}

fn http_error(status: u16) -> Error {
    Error::Transport(format!("HTTP status {status}; request was not replayed"))
}

fn header<'a>(headers: &'a [(String, String)], key: &str) -> Result<Option<&'a str>, Error> {
    let mut values = headers
        .iter()
        .filter(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.as_str());
    let value = values.next();
    if values.next().is_some() {
        return Err(Error::Protocol(format!("repeated {key} response header")));
    }
    Ok(value)
}

async fn acknowledge<T: Transport>(
    transport: &T,
    context: &mut Context,
    message: Value,
) -> Result<(), Error> {
    let mut response = transport
        .send(context.http("POST", message.to_string().into_bytes()))
        .await?;
    context.status(response.status)?;
    if response.status != 202 {
        return Err(Error::Protocol(
            "notification/response POST requires HTTP 202".into(),
        ));
    }
    while let Some(chunk) = response.body.next().await? {
        cooperate().await;
        if !chunk.is_empty() {
            return Err(Error::Protocol(
                "HTTP 202 acknowledgement must have no body".into(),
            ));
        }
    }
    Ok(())
}

async fn exchange<T: Transport>(
    transport: &T,
    context: &mut Context,
    message: Value,
    id: RpcId,
    initializing: bool,
) -> Result<Value, Error> {
    let mut response = transport
        .send(context.http("POST", message.to_string().into_bytes()))
        .await?;
    context.status(response.status)?;
    if let Some(session) = header(&response.headers, "mcp-session-id")? {
        if session.is_empty() || !session.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
            return Err(Error::Protocol(
                "invalid MCP-Session-Id response header".into(),
            ));
        }
        if initializing {
            context.session = Some(session.into());
        } else if context.session.as_deref() != Some(session) {
            return Err(Error::Protocol("server changed the MCP session ID".into()));
        }
    }
    let content_type = header(&response.headers, "content-type")?
        .and_then(|value| value.split(';').next())
        .map(str::trim)
        .unwrap_or("");
    let mut matcher = ResponseMatcher::new(Some(id));
    if content_type.eq_ignore_ascii_case("application/json") {
        let mut bytes = Vec::new();
        while let Some(chunk) = response.body.next().await? {
            cooperate().await;
            if chunk.len()
                > context
                    .limits
                    .max_response_bytes
                    .saturating_sub(bytes.len())
            {
                return Err(Error::Protocol("response body byte limit exceeded".into()));
            }
            bytes.extend_from_slice(&chunk);
        }
        let message = wire::decode_json(&bytes, context.limits.max_response_bytes)?;
        if matcher.observe(&message)? {
            return rpc_result(message, context);
        }
        return Err(Error::Protocol(
            "JSON response must be the matching RPC reply".into(),
        ));
    }
    if !content_type.eq_ignore_ascii_case("text/event-stream") {
        return Err(Error::Protocol(
            "expected application/json or text/event-stream".into(),
        ));
    }
    let mut decoder = SseDecoder::new(SseLimits {
        max_buffer_bytes: context.limits.max_event_bytes,
        max_event_bytes: context.limits.max_event_bytes,
        max_body_bytes: context.limits.max_response_bytes,
    });
    while let Some(chunk) = response.body.next().await? {
        cooperate().await;
        let mut input = chunk.as_slice();
        while let Some(event) = decoder.next_event(&mut input)? {
            cooperate().await;
            let Some(message) = event.message else {
                continue;
            };
            if event.event_type != "message" {
                return Err(Error::Protocol("unsupported SSE event type".into()));
            }
            if matcher.observe(&message)? {
                return rpc_result(message, context);
            }
            if let RpcMessage::Request { id, method, .. } = message {
                let id = match id {
                    RpcId::String(value) => Value::String(value),
                    RpcId::Number(value) => Value::Number(value),
                };
                let reply = if method == "ping" {
                    json!({"jsonrpc":"2.0","id":id,"result":{}})
                } else {
                    json!({"jsonrpc":"2.0","id":id,"error":{"code":-32601,"message":"Method not found"}})
                };
                acknowledge(transport, context, reply).await?;
            }
            // Notifications (including progress) do not reset the deadline.
        }
    }
    decoder.finish()?;
    matcher.finish()?;
    Err(Error::Protocol("stream ended without a response".into()))
}

fn rpc_result(message: RpcMessage, context: &Context) -> Result<Value, Error> {
    match message {
        RpcMessage::Success { result, .. } => Ok(Value::Object(result)),
        RpcMessage::Error { error, .. } => {
            let mut diagnostic = json!({
                "code":error.code,"message":error.message,"data":error.data
            })
            .to_string();
            for secret in [&context.bearer, &context.session].into_iter().flatten() {
                diagnostic = diagnostic.replace(secret, "[redacted]");
                let escaped = serde_json::to_string(secret).expect("string serializes");
                if let Some(escaped) = escaped.strip_prefix('"').and_then(|v| v.strip_suffix('"')) {
                    diagnostic = diagnostic.replace(escaped, "[redacted]");
                }
            }
            Err(Error::Protocol(format!("JSON-RPC error: {diagnostic}")))
        }
        _ => Err(Error::Protocol("expected RPC response".into())),
    }
}

/// Check a successful tools/call envelope before decoding structured content.
///
/// Content is preserved, not converted into strings or substituted for missing
/// structured content. This validates the envelope, not every content-block type.
pub fn tool_result(result: Value) -> Result<Value, Error> {
    if result.get("isError") == Some(&Value::Bool(true)) {
        return Err(Error::ToolExecution(result.to_string()));
    }
    if result
        .get("isError")
        .is_some_and(|value| !value.is_boolean())
        || !result.get("content").is_some_and(Value::is_array)
        || result
            .get("structuredContent")
            .is_some_and(|value| !value.is_object())
    {
        return Err(Error::Protocol("invalid CallToolResult envelope".into()));
    }
    Ok(result)
}

async fn cooperate() {
    let mut yielded = false;
    poll_fn(|cx| {
        if yielded {
            Poll::Ready(())
        } else {
            yielded = true;
            cx.waker().wake_by_ref();
            Poll::Pending
        }
    })
    .await;
}
