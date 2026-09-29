//! Bounded HTTP and strict wire decoding beneath the SDK's session worker.
//!
//! The SDK owns normal sessions and dispatch. A temporary bootstrap cleanup
//! lease, bootstrap-only inline replies, and ID-free control counters cover its
//! early-initialization and shutdown gaps without replacing that lifecycle.

use std::{
    collections::HashMap,
    fmt,
    future::Future,
    sync::{Arc, Mutex},
    time::Duration,
};

use futures_util::{StreamExt, stream};
use http::{HeaderName, HeaderValue};
use rmcp::{
    model::{
        ClientJsonRpcMessage, ClientResult, CustomNotification, CustomRequest, CustomResult,
        ErrorCode, ErrorData, JsonRpcError, JsonRpcNotification, JsonRpcRequest, JsonRpcResponse,
        RequestId, ServerJsonRpcMessage, ServerNotification, ServerRequest, ServerResult,
    },
    transport::{
        common::client_side_sse::BoxedSseResponse,
        streamable_http_client::{
            StreamableHttpClient, StreamableHttpError, StreamableHttpPostResponse,
        },
    },
};
use serde_json::{Map, Value};
use tokio::{sync::Notify, time::Instant};

use crate::{
    native::{NativeBody, NativeTransport},
    runtime::{
        Body, Error, Limits, Request, Transport,
        wire::{self, RpcError, RpcId, RpcMessage, SseDecoder, SseLimits},
    },
};

pub(crate) const RAW_TOOLS_PAGE: &str = "autostamp/raw-tools-page";

#[derive(Clone)]
pub(crate) struct SdkHttp {
    transport: NativeTransport,
    limits: Limits,
    diagnostic: Arc<Mutex<Option<Error>>>,
    lifecycle: Arc<Lifecycle>,
}

impl fmt::Debug for SdkHttp {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("SdkHttp")
            .field("limits", &self.limits)
            .finish_non_exhaustive()
    }
}

impl SdkHttp {
    pub(crate) fn new(transport: NativeTransport, limits: Limits) -> Self {
        Self {
            transport,
            limits,
            diagnostic: Arc::new(Mutex::new(None)),
            lifecycle: Arc::new(Lifecycle {
                state: Mutex::new(LifecycleState {
                    bootstrap: Bootstrap::Pending,
                    lease: None,
                    active: 0,
                    controls: 0,
                    control_deadline: None,
                    control_failure: None,
                }),
                changed: Notify::new(),
            }),
        }
    }

    pub(crate) fn error(&self) -> Option<Error> {
        self.diagnostic
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .clone()
    }

    pub(crate) fn complete_bootstrap(&self) {
        let mut state = self.lifecycle.lock();
        if state.bootstrap == Bootstrap::Pending {
            state.bootstrap = Bootstrap::Complete;
            state.lease = None;
        }
    }

    pub(crate) async fn drain(&self) -> Result<(), Error> {
        let mut guard = DiagnosticGuard::new(
            self.diagnostic.clone(),
            "waiting for SDK control POSTs was cancelled",
        );
        let result = async {
            self.limits.validate()?;
            until(
                &self.transport,
                deadline(self.limits)?,
                self.wait_for_posts(),
            )
            .await
        }
        .await;
        guard.finish(&result);
        result
    }

    pub(crate) async fn cleanup_bootstrap(&self) -> Result<(), Error> {
        {
            let mut state = self.lifecycle.lock();
            if state.bootstrap == Bootstrap::Complete {
                return Ok(());
            }
            state.bootstrap = Bootstrap::Failed;
        }
        let mut guard = DiagnosticGuard::new(
            self.diagnostic.clone(),
            "failed bootstrap cleanup was cancelled before completion",
        );
        let result = async {
            self.limits.validate()?;
            until(&self.transport, deadline(self.limits)?, async {
                self.wait_for_posts().await?;
                let lease = self.lifecycle.lock().lease.take();
                if let Some(lease) = lease
                    && let Some(session) = lease.session
                {
                    self.delete_session(lease.uri, session, lease.auth, lease.headers)
                        .await
                        .map_err(|error| match error {
                            StreamableHttpError::Client(error) => error,
                            _ => Error::Transport("failed bootstrap session cleanup".into()),
                        })?;
                }
                Ok(())
            })
            .await
        }
        .await
        .map_err(|error| Error::Transport(format!("failed bootstrap cleanup: {error}")));
        guard.finish(&result);
        result
    }

    async fn wait_for_posts(&self) -> Result<(), Error> {
        loop {
            let changed = self.lifecycle.changed.notified();
            tokio::pin!(changed);
            changed.as_mut().enable();
            let control_deadline = {
                let state = self.lifecycle.lock();
                if let Some(error) = &state.control_failure {
                    return Err(error.clone());
                }
                if state.active == 0 && state.controls == 0 {
                    return Ok(());
                }
                state.control_deadline
            };
            if let Some(deadline) = control_deadline {
                until(&self.transport, deadline, async {
                    changed.await;
                    Ok(())
                })
                .await?;
            } else {
                changed.await;
            }
        }
    }

    async fn post(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        limits: Limits,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Error>> {
        let mut guard = DiagnosticGuard::new(
            self.diagnostic.clone(),
            "HTTP POST cancelled before completion; request was not replayed",
        );
        let mut flight = None;
        let result =
            async {
                limits.validate()?;
                let mut deadline = deadline(limits)?;
                let control = matches!(
                    message,
                    ClientJsonRpcMessage::Response(_) | ClientJsonRpcMessage::Error(_)
                );
                flight = Some(Flight::start(self.lifecycle.clone(), control, true)?);
                if control && let Some(source_deadline) = self.lifecycle.lock().control_deadline {
                    deadline = deadline.min(source_deadline);
                }
                until(&self.transport, deadline, async {
                let kind = match &message {
                    ClientJsonRpcMessage::Request(request) => match request.request.method() {
                        "initialize" => ReplyKind::Initialize,
                        "tools/list" => ReplyKind::Tools,
                        _ => {
                            return Err(Error::Protocol(
                                "unsupported native discovery request method".into(),
                            ));
                        }
                    },
                    _ => ReplyKind::Acknowledgement,
                };
                let mut projection = Projection {
                    kind,
                    endpoint: uri.to_string(),
                    credentials: Vec::new(),
                };
                projection.credentials.extend(auth_header.iter().cloned());
                projection
                    .credentials
                    .extend(session_id.iter().map(ToString::to_string));
                let mut bootstrap = (kind == ReplyKind::Initialize).then(|| {
                    let mut headers = custom_headers.clone();
                    headers.insert(
                        HeaderName::from_static("mcp-protocol-version"),
                        HeaderValue::from_static(crate::runtime::PROTOCOL_VERSION),
                    );
                    BootstrapContext {
                        uri: uri.clone(),
                        session: session_id.clone(),
                        auth: auth_header.clone(),
                        headers,
                    }
                });
                let body = serde_json::to_vec(&message)
                    .map_err(|_| Error::Protocol("cannot serialize SDK request".into()))?;
                let request = request(
                    "POST",
                    uri,
                    session_id.as_deref(),
                    auth_header.as_deref(),
                    custom_headers,
                    body,
                )?;
                let mut response = self.transport.send(request).await?;
                let session = response_session(&response.headers)?;
                if let Some(session) = &session {
                    projection.credentials.push(session.clone());
                    if kind != ReplyKind::Initialize && session_id.as_deref() != Some(session) {
                        return Err(Error::Protocol("server changed the MCP session ID".into()));
                    }
                    if response.status == 200
                        && let Some(bootstrap) = &mut bootstrap
                    {
                        bootstrap.session = Some(session.clone().into());
                        let mut state = self.lifecycle.lock();
                        if state.bootstrap != Bootstrap::Complete {
                            state.lease = Some(bootstrap.clone());
                        }
                    }
                }
                let content_type = single_header(&response.headers, "content-type")?
                    .and_then(|value| value.split(';').next())
                    .map(str::trim)
                    .unwrap_or("")
                    .to_owned();
                if !(200..300).contains(&response.status) {
                    return Err(http_error(response.status));
                }
                if kind == ReplyKind::Acknowledgement {
                    if response.status != 202 {
                        return Err(Error::Protocol(
                            "notification/response POST requires HTTP 202".into(),
                        ));
                    }
                    while let Some(chunk) = response.body.next().await? {
                        check_deadline(deadline)?;
                        if !chunk.is_empty() {
                            return Err(Error::Protocol(
                                "HTTP 202 acknowledgement must have no body".into(),
                            ));
                        }
                        tokio::task::yield_now().await;
                    }
                    return Ok(StreamableHttpPostResponse::Accepted);
                }
                if response.status != 200 {
                    return Err(Error::Protocol(
                        "request POST requires HTTP 200 with a JSON-RPC response".into(),
                    ));
                }
                if content_type.eq_ignore_ascii_case("application/json") {
                    let mut bytes = Vec::new();
                    while let Some(chunk) = response.body.next().await? {
                        check_deadline(deadline)?;
                        if chunk.len() > limits.max_response_bytes.saturating_sub(bytes.len()) {
                            return Err(Error::Protocol("response body byte limit exceeded".into()));
                        }
                        bytes.extend_from_slice(&chunk);
                        tokio::task::yield_now().await;
                    }
                    let message = wire::decode_json(&bytes, limits.max_response_bytes)?;
                    if !matches!(message, RpcMessage::Success { .. } | RpcMessage::Error { .. }) {
                        return Err(Error::Protocol(
                            "JSON POST response must be a JSON-RPC reply".into(),
                        ));
                    }
                    let message = projection.message(message)?;
                    return Ok(StreamableHttpPostResponse::Json(message, session));
                }
                if !content_type.eq_ignore_ascii_case("text/event-stream") {
                    return Err(Error::Protocol(
                        "expected application/json or text/event-stream".into(),
                    ));
                }
                let state = StreamState {
                    transport: self.transport.clone(),
                    body: Some(response.body),
                    decoder: SseDecoder::new(SseLimits {
                        max_buffer_bytes: limits.max_event_bytes,
                        max_event_bytes: limits.max_event_bytes,
                        max_body_bytes: limits.max_response_bytes,
                    }),
                    chunk: Vec::new(),
                    offset: 0,
                    deadline,
                    projection,
                    bootstrap,
                    lifecycle: self.lifecycle.clone(),
                    guard: DiagnosticGuard::new(
                        self.diagnostic.clone(),
                        "HTTP response stream cancelled before its reply; request was not replayed",
                    ),
                    flight: flight.take(),
                };
                let stream = stream::try_unfold(state, |mut state| async move {
                    if state.body.is_none() {
                        return Ok(None);
                    }
                    let transport = state.transport.clone();
                    let result = until(&transport, state.deadline, state.next()).await;
                    match result {
                        Ok(Some(event)) => Ok(Some((event, state))),
                        Ok(None) => Ok(None),
                        Err(error) => {
                            state.guard.finish(&Err::<(), _>(error.clone()));
                            Err(sse_stream::Error::Body(Box::new(error)))
                        }
                    }
                })
                .boxed();
                Ok(StreamableHttpPostResponse::Sse(stream, session))
            })
            .await
            }
            .await;
        guard.finish(&result);
        if let Some(flight) = &mut flight {
            flight.finish(&result);
        }
        result.map_err(StreamableHttpError::Client)
    }
}

impl StreamableHttpClient for SdkHttp {
    type Error = Error;

    async fn post_message(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        self.post(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            self.limits,
        )
        .await
    }

    async fn post_message_with_max_sse_event_size(
        &self,
        uri: Arc<str>,
        message: ClientJsonRpcMessage,
        session_id: Option<Arc<str>>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
        max_sse_event_size: usize,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Self::Error>> {
        let limits = Limits {
            max_event_bytes: self.limits.max_event_bytes.min(max_sse_event_size),
            ..self.limits
        };
        self.post(
            uri,
            message,
            session_id,
            auth_header,
            custom_headers,
            limits,
        )
        .await
    }

    async fn delete_session(
        &self,
        uri: Arc<str>,
        session_id: Arc<str>,
        auth_header: Option<String>,
        custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<(), StreamableHttpError<Self::Error>> {
        let mut guard = DiagnosticGuard::new(
            self.diagnostic.clone(),
            "HTTP DELETE cancelled before completion; cleanup did not finish",
        );
        let mut flight = None;
        let result = async {
            self.limits.validate()?;
            flight = Some(Flight::start(self.lifecycle.clone(), false, false)?);
            {
                let mut state = self.lifecycle.lock();
                if state.lease.as_ref().is_some_and(|lease| {
                    lease.uri == uri && lease.session.as_ref() == Some(&session_id)
                }) {
                    state.lease = None;
                }
            }
            let deadline = deadline(self.limits)?;
            until(&self.transport, deadline, async {
                let request = request(
                    "DELETE",
                    uri,
                    Some(&session_id),
                    auth_header.as_deref(),
                    custom_headers,
                    Vec::new(),
                )?;
                let response = self.transport.send(request).await?;
                single_header(&response.headers, "content-type")?;
                response_session(&response.headers)?;
                if (200..300).contains(&response.status) || matches!(response.status, 404 | 405) {
                    Ok(())
                } else {
                    Err(http_error(response.status))
                }
            })
            .await
        }
        .await;
        guard.finish(&result);
        result.map_err(StreamableHttpError::Client)
    }

    async fn get_stream(
        &self,
        _uri: Arc<str>,
        _session_id: Option<Arc<str>>,
        _last_event_id: Option<String>,
        _auth_header: Option<String>,
        _custom_headers: HashMap<HeaderName, HeaderValue>,
    ) -> Result<BoxedSseResponse, StreamableHttpError<Self::Error>> {
        Err(StreamableHttpError::ServerDoesNotSupportSse)
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Bootstrap {
    Pending,
    Complete,
    Failed,
}

#[derive(Clone)]
struct BootstrapContext {
    uri: Arc<str>,
    session: Option<Arc<str>>,
    auth: Option<String>,
    headers: HashMap<HeaderName, HeaderValue>,
}

struct LifecycleState {
    bootstrap: Bootstrap,
    lease: Option<BootstrapContext>,
    active: usize,
    controls: usize,
    control_deadline: Option<Instant>,
    control_failure: Option<Error>,
}

struct Lifecycle {
    state: Mutex<LifecycleState>,
    changed: Notify,
}

impl Lifecycle {
    fn lock(&self) -> std::sync::MutexGuard<'_, LifecycleState> {
        self.state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
    }

    fn register_control(&self, deadline: Instant) {
        {
            let mut state = self.lock();
            state.controls += 1;
            // Conservative across overlapping source streams: a newer reply
            // must never extend the budget of an older, still-pending reply.
            state.control_deadline = Some(
                state
                    .control_deadline
                    .map_or(deadline, |current| current.min(deadline)),
            );
        }
        self.changed.notify_waiters();
    }
}

struct Flight {
    lifecycle: Arc<Lifecycle>,
    control: bool,
    finished: bool,
}

impl Flight {
    fn start(lifecycle: Arc<Lifecycle>, control: bool, post: bool) -> Result<Self, Error> {
        {
            let mut state = lifecycle.lock();
            if post && state.bootstrap == Bootstrap::Failed {
                return Err(Error::Transport(
                    "discovery bootstrap is closed; request was not replayed".into(),
                ));
            }
            state.active += 1;
        }
        Ok(Self {
            lifecycle,
            control,
            finished: false,
        })
    }

    fn finish<T>(&mut self, result: &Result<T, Error>) {
        self.finished = true;
        if self.control {
            let mut state = self.lifecycle.lock();
            match result {
                Ok(_) => {
                    state.controls = state.controls.saturating_sub(1);
                    if state.controls == 0 {
                        state.control_deadline = None;
                    }
                }
                Err(error) => state.control_failure = Some(error.clone()),
            }
        }
    }
}

impl Drop for Flight {
    fn drop(&mut self) {
        {
            let mut state = self.lifecycle.lock();
            state.active = state.active.saturating_sub(1);
            if self.control && !self.finished {
                state.control_failure = Some(Error::Transport(
                    "SDK control POST cancelled before acknowledgement".into(),
                ));
            }
        }
        self.lifecycle.changed.notify_waiters();
    }
}

struct DiagnosticGuard {
    diagnostic: Arc<Mutex<Option<Error>>>,
    cancelled: &'static str,
    active: bool,
}

impl DiagnosticGuard {
    fn new(diagnostic: Arc<Mutex<Option<Error>>>, cancelled: &'static str) -> Self {
        Self {
            diagnostic,
            cancelled,
            active: true,
        }
    }

    fn finish<T>(&mut self, result: &Result<T, Error>) {
        self.active = false;
        if let Err(error) = result {
            *self
                .diagnostic
                .lock()
                .unwrap_or_else(|poisoned| poisoned.into_inner()) = Some(error.clone());
        }
    }
}

impl Drop for DiagnosticGuard {
    fn drop(&mut self) {
        if self.active {
            self.finish(&Err::<(), _>(Error::Transport(self.cancelled.into())));
        }
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum ReplyKind {
    Initialize,
    Tools,
    Acknowledgement,
}

struct Projection {
    kind: ReplyKind,
    endpoint: String,
    credentials: Vec<String>,
}

impl Projection {
    fn message(&self, message: RpcMessage) -> Result<ServerJsonRpcMessage, Error> {
        match message {
            RpcMessage::Success { id, result } => {
                let result = match self.kind {
                    ReplyKind::Initialize => {
                        validate_initialize(&result)?;
                        ServerResult::InitializeResult(
                            serde_json::from_value(Value::Object(result))
                                .map_err(|_| Error::Protocol("invalid initialize result".into()))?,
                        )
                    }
                    ReplyKind::Tools => {
                        let page = Value::Object(result);
                        if contains_credential(&page, &self.credentials) {
                            return Err(Error::Protocol(
                                "discovered content contains a session or discovery credential; refusing to persist it".into(),
                            ));
                        }
                        // The SDK reparses SSE through its typed result union. A string
                        // preserves extensions, null presence and private-number objects.
                        let mut wrapper = Map::new();
                        wrapper.insert(RAW_TOOLS_PAGE.into(), Value::String(page.to_string()));
                        ServerResult::CustomResult(CustomResult::new(Value::Object(wrapper)))
                    }
                    ReplyKind::Acknowledgement => {
                        return Err(Error::Protocol("unexpected JSON-RPC response".into()));
                    }
                };
                Ok(ServerJsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: Default::default(),
                    id: sdk_id(id)?,
                    result,
                }))
            }
            // In particular, rmcp's initialization SSE helper ignores Error messages.
            // A stream/transport error fails promptly and retains the remote diagnostic.
            RpcMessage::Error { error, .. } => Err(self.rpc_error(error)),
            RpcMessage::Request { id, method, params } => {
                let request = if method == "ping" {
                    ServerRequest::PingRequest(Default::default())
                } else {
                    ServerRequest::CustomRequest(CustomRequest::new(
                        method,
                        params.map(Value::Object),
                    ))
                };
                Ok(ServerJsonRpcMessage::Request(JsonRpcRequest::new(
                    sdk_id(id)?,
                    request,
                )))
            }
            RpcMessage::Notification { method, params } => {
                Ok(ServerJsonRpcMessage::Notification(JsonRpcNotification {
                    jsonrpc: Default::default(),
                    notification: ServerNotification::CustomNotification(CustomNotification::new(
                        method,
                        params.map(Value::Object),
                    )),
                }))
            }
        }
    }

    fn rpc_error(&self, error: RpcError) -> Error {
        let mut payload = Map::new();
        payload.insert("code".into(), Value::Number(error.code));
        payload.insert("message".into(), Value::String(error.message));
        if let Some(data) = error.data {
            payload.insert("data".into(), data);
        }
        let mut diagnostic = Value::Object(payload).to_string();
        for secret in self
            .credentials
            .iter()
            .chain(std::iter::once(&self.endpoint))
            .filter(|secret| !secret.is_empty())
        {
            diagnostic = diagnostic.replace(secret, "[redacted]");
            let escaped = Value::String(secret.clone()).to_string();
            if let Some(escaped) = escaped.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
                diagnostic = diagnostic.replace(escaped, "[redacted]");
            }
        }
        Error::Protocol(format!("JSON-RPC error: {diagnostic}"))
    }
}

fn validate_initialize(result: &Map<String, Value>) -> Result<(), Error> {
    if result.get("protocolVersion").and_then(Value::as_str)
        != Some(crate::runtime::PROTOCOL_VERSION)
    {
        return Err(Error::Protocol(
            "unsupported negotiated protocol version".into(),
        ));
    }
    let tools = result
        .get("capabilities")
        .and_then(Value::as_object)
        .and_then(|capabilities| capabilities.get("tools"))
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
        info.and_then(|info| info.get(*key))
            .is_some_and(Value::is_string)
    }) {
        return Err(Error::Protocol(
            "serverInfo requires string name and version".into(),
        ));
    }
    Ok(())
}

fn contains_credential(value: &Value, credentials: &[String]) -> bool {
    let contains = |value: &str| {
        credentials
            .iter()
            .any(|secret| !secret.is_empty() && value.contains(secret))
    };
    match value {
        Value::String(value) => contains(value),
        Value::Array(values) => values
            .iter()
            .any(|value| contains_credential(value, credentials)),
        Value::Object(values) => values
            .iter()
            .any(|(key, value)| contains(key) || contains_credential(value, credentials)),
        Value::Null | Value::Bool(_) | Value::Number(_) => false,
    }
}

struct StreamState {
    transport: NativeTransport,
    body: Option<NativeBody>,
    decoder: SseDecoder,
    chunk: Vec<u8>,
    offset: usize,
    deadline: Instant,
    projection: Projection,
    bootstrap: Option<BootstrapContext>,
    lifecycle: Arc<Lifecycle>,
    guard: DiagnosticGuard,
    flight: Option<Flight>,
}

impl StreamState {
    async fn next(&mut self) -> Result<Option<sse_stream::Sse>, Error> {
        if self.body.is_none() {
            return Ok(None);
        }
        loop {
            check_deadline(self.deadline)?;
            if self.bootstrap.is_some() && self.lifecycle.lock().bootstrap == Bootstrap::Failed {
                return Err(Error::Transport(
                    "discovery bootstrap cancelled before its reply".into(),
                ));
            }
            let mut input = self.chunk.get(self.offset..).unwrap_or_default();
            let event = self.decoder.next_event(&mut input)?;
            self.offset = self.chunk.len() - input.len();
            if let Some(event) = event {
                if event.event_type != "message" {
                    return Err(Error::Protocol("unsupported SSE event type".into()));
                }
                let Some(message) = event.message else {
                    continue;
                };
                let message = match message {
                    RpcMessage::Request { id, method, .. } if self.bootstrap.is_some() => {
                        self.reply_during_bootstrap(id, &method).await?;
                        continue;
                    }
                    message => message,
                };
                let is_request = matches!(message, RpcMessage::Request { .. });
                let is_response = matches!(
                    message,
                    RpcMessage::Success { .. } | RpcMessage::Error { .. }
                );
                let message = self.projection.message(message)?;
                let data = serde_json::to_string(&message)
                    .map_err(|_| Error::Protocol("cannot serialize SDK response".into()))?;
                check_deadline(self.deadline)?;
                if is_request {
                    // Count the reply before yielding, including the interval before
                    // the SDK has dispatched it and started the response POST.
                    self.lifecycle.register_control(self.deadline);
                }
                if is_response {
                    // Do not wait for EOF, count unused trailing bytes, or expose a
                    // second response. The SDK owns correlation of the original ID.
                    self.body = None;
                    self.chunk.clear();
                    self.guard.finish(&Ok(()));
                    self.flight = None;
                }
                return Ok(Some(sse_stream::Sse {
                    data: Some(data),
                    event: Some("message".into()),
                    id: (!event.id.is_empty()).then_some(event.id),
                    retry: event.retry_ms,
                }));
            }
            let Some(body) = self.body.as_mut() else {
                return Ok(None);
            };
            match body.next().await? {
                Some(chunk) => {
                    self.chunk = chunk;
                    self.offset = 0;
                    tokio::task::yield_now().await;
                }
                None => {
                    self.decoder.finish()?;
                    return Err(wire::WireError::MissingResponse.into());
                }
            }
        }
    }

    async fn reply_during_bootstrap(&self, id: RpcId, method: &str) -> Result<(), Error> {
        let context = self
            .bootstrap
            .as_ref()
            .ok_or_else(|| Error::Protocol("missing bootstrap response context".into()))?;
        let id = sdk_id(id)?;
        let message = if method == "ping" {
            ClientJsonRpcMessage::Response(JsonRpcResponse {
                jsonrpc: Default::default(),
                id,
                result: ClientResult::empty(()),
            })
        } else {
            ClientJsonRpcMessage::Error(JsonRpcError::new(
                Some(id),
                ErrorData::new(ErrorCode::METHOD_NOT_FOUND, "Method not found", None),
            ))
        };
        let body = serde_json::to_vec(&message)
            .map_err(|_| Error::Protocol("cannot serialize SDK bootstrap response".into()))?;
        let request = request(
            "POST",
            context.uri.clone(),
            context.session.as_deref(),
            context.auth.as_deref(),
            context.headers.clone(),
            body,
        )?;
        until(&self.transport, self.deadline, async {
            let mut response = self.transport.send(request).await?;
            single_header(&response.headers, "content-type")?;
            let session = response_session(&response.headers)?;
            if session
                .as_deref()
                .is_some_and(|session| context.session.as_deref() != Some(session))
            {
                return Err(Error::Protocol("server changed the MCP session ID".into()));
            }
            if !(200..300).contains(&response.status) {
                return Err(http_error(response.status));
            }
            if response.status != 202 {
                return Err(Error::Protocol(
                    "notification/response POST requires HTTP 202".into(),
                ));
            }
            while let Some(chunk) = response.body.next().await? {
                check_deadline(self.deadline)?;
                if !chunk.is_empty() {
                    return Err(Error::Protocol(
                        "HTTP 202 acknowledgement must have no body".into(),
                    ));
                }
                tokio::task::yield_now().await;
            }
            Ok(())
        })
        .await
    }
}

fn sdk_id(id: RpcId) -> Result<RequestId, Error> {
    match id {
        RpcId::String(id) => Ok(RequestId::String(id.into())),
        RpcId::Number(id) => id.as_i64().map(RequestId::Number).ok_or_else(|| {
            Error::Protocol("JSON-RPC numeric ID exceeds the SDK's signed 64-bit range".into())
        }),
    }
}

fn request(
    method: &str,
    uri: Arc<str>,
    session: Option<&str>,
    auth: Option<&str>,
    custom_headers: HashMap<HeaderName, HeaderValue>,
    body: Vec<u8>,
) -> Result<Request, Error> {
    let mut headers = vec![(
        "accept".into(),
        "application/json, text/event-stream".into(),
    )];
    if method == "POST" {
        headers.push(("content-type".into(), "application/json".into()));
    }
    if let Some(auth) = auth {
        if auth.is_empty() || !auth.bytes().all(|byte| matches!(byte, 0x21..=0x7e)) {
            return Err(Error::Transport("invalid bearer credential".into()));
        }
        headers.push(("authorization".into(), format!("Bearer {auth}")));
    }
    if let Some(session) = session {
        headers.push(("mcp-session-id".into(), session.into()));
    }
    for (name, value) in custom_headers {
        if matches!(
            name.as_str(),
            "accept"
                | "content-type"
                | "authorization"
                | "mcp-session-id"
                | "host"
                | "content-length"
                | "transfer-encoding"
        ) {
            return Err(Error::Protocol("reserved custom HTTP header".into()));
        }
        let value = value
            .to_str()
            .map_err(|_| Error::Protocol("invalid custom HTTP header".into()))?;
        headers.push((name.to_string(), value.to_owned()));
    }
    Ok(Request {
        method: method.into(),
        endpoint: uri.to_string(),
        headers,
        body,
    })
}

fn single_header<'a>(
    headers: &'a [(String, String)],
    name: &'static str,
) -> Result<Option<&'a str>, Error> {
    let mut values = headers
        .iter()
        .filter(|(key, _)| key.eq_ignore_ascii_case(name))
        .map(|(_, value)| value.as_str());
    let first = values.next();
    if values.next().is_some() {
        return Err(Error::Protocol(format!("repeated {name} response header")));
    }
    Ok(first)
}

fn response_session(headers: &[(String, String)]) -> Result<Option<String>, Error> {
    let session = single_header(headers, "mcp-session-id")?;
    if session.is_some_and(|session| {
        session.is_empty() || !session.bytes().all(|byte| matches!(byte, 0x21..=0x7e))
    }) {
        return Err(Error::Protocol(
            "invalid MCP-Session-Id response header".into(),
        ));
    }
    Ok(session.map(str::to_owned))
}

fn http_error(status: u16) -> Error {
    Error::Transport(format!("HTTP status {status}; request was not replayed"))
}

fn timeout_error() -> Error {
    Error::Transport("total request timeout exceeded; request was not replayed".into())
}

fn deadline(limits: Limits) -> Result<Instant, Error> {
    Instant::now()
        .checked_add(Duration::from_millis(limits.request_timeout_ms))
        .ok_or_else(|| Error::Transport("native request timeout is out of range".into()))
}

fn check_deadline(deadline: Instant) -> Result<(), Error> {
    if Instant::now() >= deadline {
        Err(timeout_error())
    } else {
        Ok(())
    }
}

async fn until<T>(
    transport: &NativeTransport,
    deadline: Instant,
    future: impl Future<Output = Result<T, Error>>,
) -> Result<T, Error> {
    let remaining = deadline
        .checked_duration_since(Instant::now())
        .filter(|remaining| !remaining.is_zero())
        .ok_or_else(timeout_error)?;
    let result = transport.within(remaining, future).await;
    check_deadline(deadline)?;
    result
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::{ClientRequest, InitializedNotification};
    use tokio::{
        io::{AsyncReadExt, AsyncWriteExt},
        net::{TcpListener, TcpStream},
        sync::oneshot,
        task::JoinHandle,
    };

    const PAGE: &str = r#"{"jsonrpc":"2.0","id":7.0e0,"result":{"tools":[{"name":"probe","description":null,"inputSchema":{"type":"object","x-number":18446744073709551617,"x-tiny":1e-999,"x-marker":{"$serde_json::private::Number":"7"}},"x-tool":true}],"x-page":null}}"#;

    struct Fixture {
        uri: Arc<str>,
        task: JoinHandle<()>,
        incoming: oneshot::Receiver<String>,
    }

    async fn read_request(socket: &mut TcpStream) -> String {
        let mut bytes = Vec::new();
        loop {
            let mut chunk = [0; 1024];
            let count = socket.read(&mut chunk).await.unwrap();
            assert_ne!(count, 0);
            bytes.extend_from_slice(chunk.get(..count).unwrap());
            assert!(bytes.len() < 64 * 1024);
            let text = String::from_utf8_lossy(&bytes);
            let Some((headers, body)) = text.split_once("\r\n\r\n") else {
                continue;
            };
            let length: usize = headers
                .lines()
                .filter_map(|line| line.split_once(':'))
                .find(|(key, _)| key.eq_ignore_ascii_case("content-length"))
                .map_or(0, |(_, value)| value.trim().parse().unwrap());
            if body.len() >= length {
                return String::from_utf8(bytes).unwrap();
            }
        }
    }

    impl Fixture {
        async fn new(response: String, close: bool) -> Self {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let uri = format!("http://{}/mcp", listener.local_addr().unwrap()).into();
            let (sender, incoming) = oneshot::channel();
            let task = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let _ = sender.send(read_request(&mut socket).await);
                socket.write_all(response.as_bytes()).await.unwrap();
                if !close {
                    std::future::pending::<()>().await;
                }
            });
            Self {
                uri,
                task,
                incoming,
            }
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            self.task.abort();
        }
    }

    fn response(status: u16, headers: &str, body: &str) -> String {
        format!(
            "HTTP/1.1 {status} Test\r\nContent-Length: {}\r\n{headers}\r\n{body}",
            body.len()
        )
    }

    fn http(limits: Limits) -> SdkHttp {
        SdkHttp::new(NativeTransport::new(128).unwrap(), limits)
    }

    fn tools() -> ClientJsonRpcMessage {
        ClientJsonRpcMessage::Request(JsonRpcRequest::new(
            RequestId::Number(7),
            ClientRequest::CustomRequest(CustomRequest::new("tools/list", None)),
        ))
    }

    fn initialize() -> ClientJsonRpcMessage {
        ClientJsonRpcMessage::Request(JsonRpcRequest::new(
            RequestId::Number(7),
            ClientRequest::CustomRequest(CustomRequest::new("initialize", None)),
        ))
    }

    fn notification() -> ClientJsonRpcMessage {
        ClientJsonRpcMessage::notification(InitializedNotification::default().into())
    }

    async fn post(
        client: &SdkHttp,
        fixture: &Fixture,
        message: ClientJsonRpcMessage,
    ) -> Result<StreamableHttpPostResponse, StreamableHttpError<Error>> {
        client
            .post_message(fixture.uri.clone(), message, None, None, HashMap::new())
            .await
    }

    fn page(message: ServerJsonRpcMessage) -> Value {
        let ServerJsonRpcMessage::Response(reply) = message else {
            panic!("expected response");
        };
        assert_eq!(reply.id, RequestId::Number(7));
        let ServerResult::CustomResult(result) = reply.result else {
            panic!("expected lossless wrapper");
        };
        wire::parse_value(result.0.get(RAW_TOOLS_PAGE).unwrap().as_str().unwrap()).unwrap()
    }

    #[tokio::test]
    async fn json_and_sse_preserve_tools_pages_and_exact_ids() {
        let RpcMessage::Success { result, .. } = wire::decode_json(PAGE.as_bytes(), 4096).unwrap()
        else {
            panic!("expected strict result");
        };
        let expected = Value::Object(result);
        for sse in [false, true] {
            let client = http(Limits::default());
            let body = if sse {
                format!("data: {PAGE}\n\n")
            } else {
                PAGE.into()
            };
            let mime = if sse {
                "text/event-stream"
            } else {
                "application/json"
            };
            let fixture = Fixture::new(
                response(200, &format!("Content-Type: {mime}\r\n"), &body),
                false,
            )
            .await;
            let message = match post(&client, &fixture, tools()).await.unwrap() {
                StreamableHttpPostResponse::Json(message, _) => message,
                StreamableHttpPostResponse::Sse(mut stream, _) => {
                    let event = stream.next().await.unwrap().unwrap();
                    assert!(stream.next().await.is_none());
                    serde_json::from_str(event.data.as_deref().unwrap()).unwrap()
                }
                _ => panic!("expected RPC reply"),
            };
            assert_eq!(page(message), expected);
            assert_eq!(client.error(), None);
        }
    }

    #[tokio::test]
    async fn initialization_is_validated_before_sdk_acknowledgement() {
        let valid = serde_json::json!({
            "protocolVersion": crate::runtime::PROTOCOL_VERSION,
            "capabilities": {"tools": {}},
            "serverInfo": {"name": "probe", "version": "1"}
        });
        let mut future_version = valid.clone();
        future_version
            .as_object_mut()
            .unwrap()
            .insert("protocolVersion".into(), Value::String("2099-01-01".into()));
        let mut missing_tools = valid.clone();
        missing_tools
            .as_object_mut()
            .unwrap()
            .insert("capabilities".into(), serde_json::json!({}));
        let mut malformed_tools = valid.clone();
        malformed_tools
            .as_object_mut()
            .unwrap()
            .insert("capabilities".into(), serde_json::json!({"tools": []}));
        let mut malformed_list_changed = valid.clone();
        malformed_list_changed.as_object_mut().unwrap().insert(
            "capabilities".into(),
            serde_json::json!({"tools": {"listChanged": null}}),
        );
        let mut malformed_info = valid.clone();
        malformed_info.as_object_mut().unwrap().insert(
            "serverInfo".into(),
            serde_json::json!({"name": "probe", "version": 1}),
        );
        let cases = [
            (valid, None),
            (
                future_version,
                Some("unsupported negotiated protocol version"),
            ),
            (
                missing_tools,
                Some("server must declare the tools capability"),
            ),
            (
                malformed_tools,
                Some("server must declare the tools capability"),
            ),
            (
                malformed_list_changed,
                Some("tools.listChanged must be a boolean"),
            ),
            (
                malformed_info,
                Some("serverInfo requires string name and version"),
            ),
        ];
        for sse in [false, true] {
            for (result, expected_error) in &cases {
                let client = http(Limits::default());
                let body =
                    serde_json::json!({"jsonrpc": "2.0", "id": 7, "result": result}).to_string();
                let (mime, body) = if sse {
                    ("text/event-stream", format!("data: {body}\n\n"))
                } else {
                    ("application/json", body)
                };
                let fixture = Fixture::new(
                    response(
                        200,
                        &format!("Content-Type: {mime}\r\nMCP-Session-Id: issued-session\r\n"),
                        &body,
                    ),
                    false,
                )
                .await;
                let result = post(&client, &fixture, initialize()).await;
                let succeeded = if sse {
                    let StreamableHttpPostResponse::Sse(mut stream, _) = result.unwrap() else {
                        panic!("expected SSE");
                    };
                    let succeeded = stream.next().await.unwrap().is_ok();
                    assert!(stream.next().await.is_none());
                    succeeded
                } else {
                    result.is_ok()
                };
                assert_eq!(succeeded, expected_error.is_none());
                assert_eq!(
                    client.error(),
                    expected_error.map(|message| Error::Protocol(message.into()))
                );
            }
        }
    }

    #[tokio::test]
    async fn sdk_never_sends_an_acknowledgement_for_an_unsupported_version() {
        use rmcp::{
            ClientLifecycleMode, ClientServiceExt,
            transport::{
                common::client_side_sse::NeverRetry,
                streamable_http_client::{
                    StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
                },
            },
        };

        for sse in [false, true] {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let uri = format!("http://{}/mcp", listener.local_addr().unwrap());
            let server = tokio::spawn(async move {
                let (mut socket, _) = listener.accept().await.unwrap();
                let request = read_request(&mut socket).await;
                let request = wire::parse_value(request.split_once("\r\n\r\n").unwrap().1).unwrap();
                assert_eq!(
                    request.get("method").and_then(Value::as_str),
                    Some("initialize")
                );
                let body = serde_json::json!({
                    "jsonrpc": "2.0", "id": request.get("id").unwrap(),
                    "result": {
                        "protocolVersion": "2099-01-01",
                        "capabilities": {"tools": {}},
                        "serverInfo": {"name": "probe", "version": "1"}
                    }
                })
                .to_string();
                let (mime, body) = if sse {
                    ("text/event-stream", format!("data: {body}\n\n"))
                } else {
                    ("application/json", body)
                };
                socket
                    .write_all(
                        response(
                            200,
                            &format!("Content-Type: {mime}\r\nMCP-Session-Id: issued-session\r\n"),
                            &body,
                        )
                        .as_bytes(),
                    )
                    .await
                    .unwrap();
                drop(socket);
                let (mut socket, _) =
                    tokio::time::timeout(Duration::from_secs(1), listener.accept())
                        .await
                        .unwrap()
                        .unwrap();
                let cleanup = read_request(&mut socket).await.to_ascii_lowercase();
                assert!(cleanup.starts_with("delete "));
                assert!(cleanup.contains("mcp-session-id: issued-session"));
                assert!(cleanup.contains(&format!(
                    "mcp-protocol-version: {}",
                    crate::runtime::PROTOCOL_VERSION
                )));
                socket
                    .write_all(response(204, "", "").as_bytes())
                    .await
                    .unwrap();
                drop(socket);
                assert!(
                    tokio::time::timeout(Duration::from_millis(100), listener.accept())
                        .await
                        .is_err(),
                    "unsupported initialization must not be acknowledged or retried"
                );
            });
            let client = http(Limits::default());
            let mut config = StreamableHttpClientTransportConfig::with_uri(uri);
            config.retry_config = Arc::new(NeverRetry::default());
            config.reinit_on_expired_session = false;
            let transport = StreamableHttpClientTransport::with_client(client.clone(), config);
            let initialized = tokio::time::timeout(
                Duration::from_secs(1),
                ().serve_with_lifecycle(transport, ClientLifecycleMode::Initialize),
            )
            .await
            .unwrap();
            assert!(initialized.is_err());
            client.cleanup_bootstrap().await.unwrap();
            assert_eq!(
                client.error(),
                Some(Error::Protocol(
                    "unsupported negotiated protocol version".into()
                ))
            );
            server.await.unwrap();
        }
    }

    #[tokio::test]
    async fn sse_yields_requests_and_stops_before_invalid_trailing_bytes() {
        let prefix = concat!(
            "data: {\"jsonrpc\":\"2.0\",\"id\":9e0,\"method\":\"ping\"}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":\"other\",\"method\":\"unknown\",\"params\":{}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"method\":\"notifications/unknown\"}\n\n"
        );
        let content = format!("{prefix}data: {PAGE}\n\n");
        let limits = Limits {
            max_response_bytes: content.len(),
            max_event_bytes: content.len(),
            ..Limits::default()
        };
        let client = http(limits);
        let fixture = Fixture::new(
            format!(
                "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n{content}{}",
                "trailing-invalid".repeat(1024)
            ),
            false,
        )
        .await;
        let StreamableHttpPostResponse::Sse(mut stream, _) =
            post(&client, &fixture, tools()).await.unwrap()
        else {
            panic!("expected SSE");
        };
        let mut messages = Vec::new();
        while let Some(event) = stream.next().await {
            messages.push(
                serde_json::from_str::<ServerJsonRpcMessage>(
                    event.unwrap().data.as_deref().unwrap(),
                )
                .unwrap(),
            );
        }
        assert_eq!(messages.len(), 4);
        assert!(matches!(
            messages.first(),
            Some(ServerJsonRpcMessage::Request(JsonRpcRequest {
                id: RequestId::Number(9),
                request: ServerRequest::PingRequest(_),
                ..
            }))
        ));
        assert!(matches!(
            messages.get(1),
            Some(ServerJsonRpcMessage::Request(JsonRpcRequest {
                request: ServerRequest::CustomRequest(_),
                ..
            }))
        ));
        assert!(matches!(
            messages.get(2),
            Some(ServerJsonRpcMessage::Notification(_))
        ));
        page(messages.pop().unwrap());
        assert_eq!(client.error(), None);
    }

    #[tokio::test]
    async fn sdk_headers_are_forwarded_without_retaining_session_state() {
        let client = http(Limits::default());
        let mut fixture = Fixture::new(
            response(
                200,
                "Content-Type: application/json\r\nMCP-Session-Id: session-secret\r\n",
                PAGE,
            ),
            false,
        )
        .await;
        let headers = HashMap::from([(
            HeaderName::from_static("mcp-protocol-version"),
            HeaderValue::from_static("2025-11-25"),
        )]);
        client
            .post_message(
                fixture.uri.clone(),
                tools(),
                Some("session-secret".into()),
                Some("bearer-secret".into()),
                headers,
            )
            .await
            .unwrap();
        let incoming = (&mut fixture.incoming).await.unwrap().to_ascii_lowercase();
        for header in [
            "accept: application/json, text/event-stream",
            "content-type: application/json",
            "mcp-protocol-version: 2025-11-25",
            "mcp-session-id: session-secret",
            "authorization: bearer bearer-secret",
        ] {
            assert!(incoming.contains(header), "{header}");
        }
        let debug = format!("{client:?}");
        assert!(!debug.contains("secret"));
        assert!(!debug.contains(fixture.uri.as_ref()));
        assert_eq!(client.clone().error(), None);
    }

    #[tokio::test]
    async fn acknowledgements_are_strictly_202_and_empty() {
        for (status, body, accepted) in [(202, "", true), (202, " ", false), (200, "", false)] {
            let client = http(Limits::default());
            let fixture = Fixture::new(response(status, "", body), false).await;
            let result = post(&client, &fixture, notification()).await;
            assert_eq!(result.is_ok(), accepted);
            assert_eq!(client.error().is_none(), accepted);
        }
        let client = http(Limits::default());
        let fixture = Fixture::new(response(202, "", ""), false).await;
        assert!(post(&client, &fixture, tools()).await.is_err());
    }

    #[tokio::test]
    async fn rejects_ambiguous_headers_status_and_changed_sessions_without_payloads() {
        for (status, headers, session) in [
            (
                200,
                "Content-Type: application/json\r\nContent-Type: secret-mime\r\n",
                None,
            ),
            (
                200,
                "Content-Type: application/json\r\nMCP-Session-Id: first-secret\r\nMCP-Session-Id: second-secret\r\n",
                None,
            ),
            (
                200,
                "Content-Type: application/json\r\nMCP-Session-Id: changed-secret\r\n",
                Some("original-secret"),
            ),
            (302, "Location: https://secret.invalid/\r\n", None),
            (503, "Content-Type: text/html\r\n", None),
            (200, "Content-Type: secret-mime\r\n", None),
        ] {
            let client = http(Limits::default());
            let fixture = Fixture::new(response(status, headers, "secret-body"), false).await;
            let error = client
                .post_message(
                    fixture.uri.clone(),
                    tools(),
                    session.map(Into::into),
                    Some("secret-token".into()),
                    HashMap::new(),
                )
                .await
                .unwrap_err();
            let diagnostic = format!("{error:?} {:?}", client.error());
            assert!(!diagnostic.contains("secret"));
            assert!(!diagnostic.contains(fixture.uri.as_ref()));
        }
    }

    #[tokio::test]
    async fn rpc_errors_preserve_code_message_data_and_redact_escaped_secrets() {
        for sse in [false, true] {
            let client = http(Limits::default());
            let token = "token-\"\\secret";
            let session = "session-\"\\secret";
            let body = serde_json::json!({
                "jsonrpc":"2.0", "id":7,
                "error":{"code":-32042, "message":format!("failed {token}"),
                    "data":{"why":"actionable", "session":session}}
            })
            .to_string();
            let response = if sse {
                format!(
                    "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\ndata: {body}\n\n"
                )
            } else {
                response(200, "Content-Type: application/json\r\n", &body)
            };
            let fixture = Fixture::new(response, false).await;
            let result = client
                .post_message(
                    fixture.uri.clone(),
                    initialize(),
                    Some(session.into()),
                    Some(token.into()),
                    HashMap::new(),
                )
                .await;
            if sse {
                let StreamableHttpPostResponse::Sse(mut stream, _) = result.unwrap() else {
                    panic!("expected SSE");
                };
                assert!(stream.next().await.unwrap().is_err());
                assert!(stream.next().await.is_none());
            } else {
                assert!(result.is_err());
            }
            let error = client.error().unwrap().to_string();
            for retained in ["-32042", "failed", "actionable", "[redacted]"] {
                assert!(error.contains(retained), "{error}");
            }
            for secret in [token, session, "token-", "session-"] {
                assert!(!error.contains(secret), "{error}");
            }
        }
    }

    #[tokio::test]
    async fn credential_reflections_are_rejected_before_reaching_sdk_messages() {
        for sse in [false, true] {
            for page in [
                r#"{"tools":[],"_meta":{"nested":["prefix bearer-secret suffix"]}}"#,
                r#"{"tools":[],"_meta":{"session-secret":{"ok":true}}}"#,
                r#"{"tools":[],"_meta":{"nested":"bearer\u002dsecret"}}"#,
            ] {
                let client = http(Limits::default());
                let body = format!(r#"{{"jsonrpc":"2.0","id":7,"result":{page}}}"#);
                let (mime, body) = if sse {
                    ("text/event-stream", format!("data: {body}\n\n"))
                } else {
                    ("application/json", body)
                };
                let fixture = Fixture::new(
                    response(200, &format!("Content-Type: {mime}\r\n"), &body),
                    false,
                )
                .await;
                let result = client
                    .post_message(
                        fixture.uri.clone(),
                        tools(),
                        Some("session-secret".into()),
                        Some("bearer-secret".into()),
                        HashMap::new(),
                    )
                    .await;
                if sse {
                    let StreamableHttpPostResponse::Sse(mut stream, _) = result.unwrap() else {
                        panic!("expected SSE");
                    };
                    assert!(stream.next().await.unwrap().is_err());
                    assert!(stream.next().await.is_none());
                } else {
                    assert!(result.is_err());
                }
                assert_eq!(
                    client.error(),
                    Some(Error::Protocol(
                        "discovered content contains a session or discovery credential; refusing to persist it"
                            .into()
                    ))
                );
            }
        }
    }

    #[test]
    fn credential_filter_scans_only_decoded_page_keys_and_strings() {
        let json = r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[],"number":123}}"#;
        for token in ["serde_json", RAW_TOOLS_PAGE, "jsonrpc", "\"", "123"] {
            let projection = Projection {
                kind: ReplyKind::Tools,
                endpoint: String::new(),
                credentials: vec![token.into()],
            };
            let message = projection
                .message(wire::decode_json(json.as_bytes(), 4096).unwrap())
                .unwrap();
            assert_eq!(
                page(message),
                wire::parse_value(r#"{"tools":[],"number":123}"#).unwrap()
            );
        }
        let projection = Projection {
            kind: ReplyKind::Tools,
            endpoint: "http://example.test/mcp".into(),
            credentials: vec!["session-secret".into()],
        };
        let json =
            r#"{"jsonrpc":"2.0","id":7,"result":{"tools":[],"http://example.test/mcp":null}}"#;
        assert!(
            projection
                .message(wire::decode_json(json.as_bytes(), 4096).unwrap())
                .is_ok()
        );
    }

    #[tokio::test]
    async fn strict_failures_and_sse_eof_are_recorded() {
        let bodies = [
            "",
            "data: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n",
            "data: invalid\n\n",
            "event: unsupported\ndata: {\"jsonrpc\":\"2.0\",\"id\":7,\"result\":{}}\n\n",
            "data: {\"jsonrpc\":\"2.0\",\"id\":18446744073709551615,\"method\":\"ping\"}\n\n",
        ];
        for body in bodies {
            let client = http(Limits::default());
            let fixture = Fixture::new(
                response(200, "Content-Type: text/event-stream\r\n", body),
                true,
            )
            .await;
            let StreamableHttpPostResponse::Sse(mut stream, _) =
                post(&client, &fixture, tools()).await.unwrap()
            else {
                panic!("expected SSE");
            };
            assert!(stream.next().await.unwrap().is_err());
            assert!(stream.next().await.is_none());
            assert!(client.error().is_some());
        }
    }

    #[tokio::test]
    async fn body_and_event_limits_are_enforced_at_the_byte_boundary() {
        for (sse, sdk_event_limit) in [(false, None), (true, None), (true, Some(16))] {
            let limits = Limits {
                max_response_bytes: if sdk_event_limit.is_some() { 4096 } else { 64 },
                max_event_bytes: if sdk_event_limit.is_some() { 4096 } else { 64 },
                ..Limits::default()
            };
            let client = http(limits);
            let body = if sse {
                format!("data: {PAGE}\n\n")
            } else {
                PAGE.into()
            };
            let mime = if sse {
                "text/event-stream"
            } else {
                "application/json"
            };
            let fixture = Fixture::new(
                response(200, &format!("Content-Type: {mime}\r\n"), &body),
                false,
            )
            .await;
            let result = if let Some(limit) = sdk_event_limit {
                client
                    .post_message_with_max_sse_event_size(
                        fixture.uri.clone(),
                        tools(),
                        None,
                        None,
                        HashMap::new(),
                        limit,
                    )
                    .await
            } else {
                post(&client, &fixture, tools()).await
            };
            if sse {
                let StreamableHttpPostResponse::Sse(mut stream, _) = result.unwrap() else {
                    panic!("expected SSE");
                };
                assert!(stream.next().await.unwrap().is_err());
            } else {
                assert!(result.is_err());
            }
            assert!(client.error().unwrap().to_string().contains("limit"));
        }
    }

    #[tokio::test]
    async fn deadline_covers_lazy_sse_and_incomplete_json_reads() {
        for sse in [false, true] {
            let client = http(Limits {
                request_timeout_ms: 30,
                ..Limits::default()
            });
            let mime = if sse {
                "text/event-stream"
            } else {
                "application/json"
            };
            let fixture = Fixture::new(
                format!("HTTP/1.1 200 OK\r\nContent-Type: {mime}\r\n\r\n"),
                false,
            )
            .await;
            if sse {
                let StreamableHttpPostResponse::Sse(mut stream, _) =
                    post(&client, &fixture, tools()).await.unwrap()
                else {
                    panic!("expected SSE");
                };
                tokio::time::sleep(Duration::from_millis(40)).await;
                assert!(stream.next().await.unwrap().is_err());
            } else {
                assert!(post(&client, &fixture, tools()).await.is_err());
            }
            assert!(client.error().unwrap().to_string().contains("timeout"));
        }
    }

    #[tokio::test]
    async fn cleanup_failures_and_cancellation_are_visible_across_clones() {
        for status in [200, 204, 404, 405, 500] {
            let client = http(Limits::default());
            let fixture = Fixture::new(response(status, "", "secret-body"), false).await;
            let result = client
                .delete_session(fixture.uri.clone(), "session".into(), None, HashMap::new())
                .await;
            assert_eq!(result.is_ok(), status != 500);
            assert_eq!(client.clone().error().is_none(), status != 500);
        }
        let client = http(Limits::default());
        let fixture = Fixture::new(String::new(), false).await;
        let result = tokio::time::timeout(
            Duration::from_millis(20),
            client.delete_session(fixture.uri.clone(), "session".into(), None, HashMap::new()),
        )
        .await;
        assert!(result.is_err());
        assert!(
            client
                .error()
                .unwrap()
                .to_string()
                .contains("DELETE cancelled")
        );
    }

    #[tokio::test]
    async fn successful_bootstrap_hands_off_the_recovery_lease() {
        let client = http(Limits::default());
        let body = serde_json::json!({
            "jsonrpc": "2.0", "id": 7,
            "result": {
                "protocolVersion": crate::runtime::PROTOCOL_VERSION,
                "capabilities": {"tools": {}},
                "serverInfo": {"name": "probe", "version": "1"}
            }
        })
        .to_string();
        let fixture = Fixture::new(
            response(
                200,
                "Content-Type: application/json\r\nMCP-Session-Id: issued-session\r\n",
                &body,
            ),
            false,
        )
        .await;
        post(&client, &fixture, initialize()).await.unwrap();
        assert!(client.lifecycle.lock().lease.is_some());
        client.complete_bootstrap();
        assert!(client.lifecycle.lock().lease.is_none());
        assert!(client.lifecycle.lock().bootstrap == Bootstrap::Complete);
        client.cleanup_bootstrap().await.unwrap();
        assert_eq!(client.error(), None);
    }

    #[tokio::test]
    async fn drain_covers_the_gap_before_the_sdk_starts_a_control_post() {
        let client = http(Limits {
            request_timeout_ms: 300,
            ..Limits::default()
        });
        let fixture = Fixture::new(response(202, "", ""), false).await;
        client
            .lifecycle
            .register_control(Instant::now() + Duration::from_millis(300));
        let sender = client.clone();
        let uri = fixture.uri.clone();
        let task = tokio::spawn(async move {
            tokio::time::sleep(Duration::from_millis(25)).await;
            sender
                .post_message(
                    uri,
                    ClientJsonRpcMessage::Response(JsonRpcResponse {
                        jsonrpc: Default::default(),
                        id: RequestId::Number(9),
                        result: ClientResult::empty(()),
                    }),
                    None,
                    None,
                    HashMap::new(),
                )
                .await
                .unwrap();
        });
        let start = Instant::now();
        client.drain().await.unwrap();
        assert!(start.elapsed() >= Duration::from_millis(25));
        task.await.unwrap();
        assert_eq!(client.lifecycle.lock().controls, 0);
        assert_eq!(client.lifecycle.lock().active, 0);
        assert!(client.lifecycle.lock().control_deadline.is_none());
        assert_eq!(client.error(), None);

        client
            .lifecycle
            .register_control(Instant::now() + Duration::from_millis(50));
        assert!(client.drain().await.is_err());
        assert!(client.error().unwrap().to_string().contains("timeout"));
    }

    #[tokio::test]
    async fn queued_control_deadlines_do_not_reset_when_the_sdk_starts_its_post() {
        let client = http(Limits::default());
        let mut fixture = Fixture::new(response(202, "", ""), false).await;
        client.lifecycle.register_control(Instant::now());
        let drain = tokio::time::timeout(Duration::from_millis(100), client.drain())
            .await
            .unwrap();
        assert!(drain.is_err());
        assert_eq!(client.lifecycle.lock().controls, 1);

        let result = client
            .post_message(
                fixture.uri.clone(),
                ClientJsonRpcMessage::Response(JsonRpcResponse {
                    jsonrpc: Default::default(),
                    id: RequestId::Number(9),
                    result: ClientResult::empty(()),
                }),
                None,
                None,
                HashMap::new(),
            )
            .await;
        assert!(result.is_err());
        assert!(fixture.incoming.try_recv().is_err());
        assert_eq!(client.lifecycle.lock().controls, 1);
        assert!(client.lifecycle.lock().control_failure.is_some());
        assert!(client.drain().await.is_err());
        assert!(client.error().unwrap().to_string().contains("timeout"));
    }

    #[tokio::test]
    async fn dropping_unfinished_stream_records_cancellation_and_get_does_no_io() {
        let client = http(Limits::default());
        let fixture = Fixture::new(
            "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\n\r\n".into(),
            false,
        )
        .await;
        drop(post(&client, &fixture, tools()).await.unwrap());
        assert!(client.error().unwrap().to_string().contains("cancelled"));

        let client = http(Limits::default());
        assert!(matches!(
            client
                .get_stream("not even a URI".into(), None, None, None, HashMap::new(),)
                .await,
            Err(StreamableHttpError::ServerDoesNotSupportSse)
        ));
        assert_eq!(client.error(), None);
    }

    #[test]
    fn projection_preserves_actual_ids_and_rejects_sdk_overflow() {
        let projection = Projection {
            kind: ReplyKind::Tools,
            endpoint: String::new(),
            credentials: Vec::new(),
        };
        for token in ["-9223372036854775808", "8.0", "8e0", "\"actual\""] {
            let json = format!(r#"{{"jsonrpc":"2.0","id":{token},"result":{{}}}}"#);
            let message = projection
                .message(wire::decode_json(json.as_bytes(), 4096).unwrap())
                .unwrap();
            let ServerJsonRpcMessage::Response(response) = message else {
                panic!("expected response");
            };
            assert_ne!(response.id, RequestId::Number(7));
        }
        let json = r#"{"jsonrpc":"2.0","id":18446744073709551615,"result":{}}"#;
        assert!(
            projection
                .message(wire::decode_json(json.as_bytes(), 4096).unwrap())
                .is_err()
        );
    }
}
