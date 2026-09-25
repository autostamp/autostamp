//! Bounded, synchronous MCP 2025-11-25 JSON-RPC and SSE decoding.
//!
//! This module has no transport, session, reconnect, or method-specific schema logic.
//! In particular, tool `isError` is ordinary result data. Its only dependencies are
//! `serde` and `serde_json`.
//!
//! # Consumption contract
//!
//! [`SseDecoder::next_event`] consumes only through one dispatched event. Repeatedly
//! pass the remaining input, handle requests/notifications, and **stop immediately**
//! when [`ResponseMatcher::observe`] returns `true`. Do not feed unused trailing
//! bytes or wait for EOF after that point. Chunk size does not affect any limit.
//! On real EOF before a response, call `decoder.finish()?` and then
//! `matcher.finish()?` to distinguish truncated framing from a missing response.
//!
//! ```
//! use autostamp_mcp::runtime::wire::{ResponseMatcher, RpcId, SseDecoder, SseLimits};
//!
//! let mut decoder = SseDecoder::new(SseLimits {
//!     max_buffer_bytes: 1024,
//!     max_event_bytes: 4096,
//!     max_body_bytes: 16384,
//! });
//! let mut matcher = ResponseMatcher::new(Some(RpcId::Number(0.into())));
//! let mut input = b"data: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{}}\n\ndata: unfinished"
//!     .as_slice();
//! while let Some(event) = decoder.next_event(&mut input)? {
//!     if let Some(message) = event.message {
//!         // Handle server requests and notifications here.
//!         if matcher.observe(&message)? {
//!             break; // Return the response, even if the HTTP connection is still open.
//!         }
//!     }
//! }
//! assert_eq!(input, b"data: unfinished");
//! matcher.finish()?;
//! # Ok::<(), autostamp_mcp::runtime::wire::WireError>(())
//! ```

use serde::de::{self, Deserializer, MapAccess, SeqAccess, Visitor};
use serde_json::{Map, Number, Value, value::RawValue};
use std::fmt;

/// The resource whose configured byte limit was exceeded.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Limit {
    /// A complete JSON body, or the consumed portion of an SSE body.
    Body,
    /// One physical SSE line, excluding its line ending.
    Buffer,
    /// The nonblank lines in one SSE event block, including their line endings.
    Event,
}

/// A wire failure with no untrusted payload included in its diagnostic.
///
/// RPC error messages/data are deliberately preserved in [`RpcError`], not
/// interpolated into these framing and validation errors.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum WireError {
    /// A configured byte limit would be exceeded.
    LimitExceeded {
        /// The exhausted resource.
        limit: Limit,
        /// Its configured maximum, in bytes.
        maximum: usize,
    },
    /// Input is not valid UTF-8.
    InvalidUtf8,
    /// JSON syntax is invalid; the underlying error text is intentionally omitted.
    InvalidJson {
        /// One-based line number reported by `serde_json`.
        line: usize,
        /// Column reported by `serde_json`.
        column: usize,
    },
    /// The JSON-RPC envelope is malformed.
    InvalidEnvelope(&'static str),
    /// A syntactically valid SSE retry value does not fit `u64` milliseconds.
    RetryOverflow,
    /// EOF arrived during a BOM, line, or unterminated nonempty SSE block.
    TruncatedSse,
    /// EOF arrived without the expected response.
    MissingResponse,
    /// A response arrived on an unsolicited stream, or after an earlier response.
    UnexpectedResponse,
    /// An error response lacks the ID needed to correlate it to a request.
    UncorrelatedResponse,
    /// A response ID differs from the outstanding request ID.
    MismatchedResponseId,
    /// A decoder was used after `finish`.
    DecoderFinished,
    /// A decoder was used after a decoding failure.
    DecoderFailed,
}

impl fmt::Display for WireError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::LimitExceeded { limit, maximum } => {
                write!(f, "{limit:?} byte limit exceeded (maximum {maximum})")
            }
            Self::InvalidUtf8 => f.write_str("wire input must be valid UTF-8"),
            Self::InvalidJson { line, column } => {
                write!(f, "invalid JSON at line {line}, column {column}")
            }
            Self::InvalidEnvelope(reason) => write!(f, "invalid JSON-RPC envelope: {reason}"),
            Self::RetryOverflow => f.write_str("SSE retry exceeds u64 milliseconds"),
            Self::TruncatedSse => f.write_str(
                "truncated SSE input: finish the BOM, line, and blank-line event delimiter",
            ),
            Self::MissingResponse => {
                f.write_str("stream ended before the expected JSON-RPC response")
            }
            Self::UnexpectedResponse => f.write_str("JSON-RPC response has no outstanding request"),
            Self::UncorrelatedResponse => {
                f.write_str("JSON-RPC error has no ID and cannot satisfy the outstanding request")
            }
            Self::MismatchedResponseId => {
                f.write_str("JSON-RPC response ID does not match the outstanding request")
            }
            Self::DecoderFinished => f.write_str("SSE decoder is finished; create a new decoder"),
            Self::DecoderFailed => f.write_str("SSE decoder previously failed; discard it"),
        }
    }
}

impl std::error::Error for WireError {}

/// A non-null MCP request ID.
///
/// Numeric IDs are normalized exactly to integers within `i64` or `u64`.
/// Integral decimal/exponent encodings are accepted without floating-point
/// rounding; fractional values and integers outside that range are rejected.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum RpcId {
    /// A string ID (including an empty string).
    String(String),
    /// An integer ID; callers constructing IDs must use integer `Number`s.
    Number(Number),
}

impl RpcId {
    fn decode(value: Value) -> Result<Self, WireError> {
        match value {
            Value::String(value) => Ok(Self::String(value)),
            Value::Number(value) => {
                rpc_integer(&value)
                    .map(Self::Number)
                    .ok_or(WireError::InvalidEnvelope(
                        "id must be a string or an integer within i64/u64 range",
                    ))
            }
            _ => Err(WireError::InvalidEnvelope(
                "id must be a string or an integer encoded within i64/u64 range",
            )),
        }
    }
}

/// A well-formed remote JSON-RPC error, distinct from malformed wire input.
#[derive(Clone, Debug, PartialEq)]
pub struct RpcError {
    /// Integer error code, represented without narrowing unsigned values.
    pub code: Number,
    /// Remote error message. This is intentional payload, potentially sensitive.
    pub message: String,
    /// Optional remote data; absent and explicit JSON `null` remain distinct.
    pub data: Option<Value>,
}

/// One classified MCP JSON-RPC message.
///
/// Unknown envelope/error extension fields are ignored, but reserved fields
/// belonging to a different message class are errors. Method-specific payload
/// validation belongs to the caller.
#[derive(Clone, Debug, PartialEq)]
pub enum RpcMessage {
    /// A successful response.
    Success {
        /// Request ID to correlate.
        id: RpcId,
        /// MCP results must be JSON objects.
        result: Map<String, Value>,
    },
    /// An error response.
    Error {
        /// Absent only for protocol errors where the request ID could not be read.
        id: Option<RpcId>,
        /// The remote error payload.
        error: RpcError,
    },
    /// A server request, including `ping` or an unsupported method.
    Request {
        /// ID needed to send a response.
        id: RpcId,
        /// Method name; this wire layer does not restrict the method vocabulary.
        method: String,
        /// Optional named parameters; MCP does not use positional parameters.
        params: Option<Map<String, Value>>,
    },
    /// A notification, which must not be answered.
    Notification {
        /// Notification method.
        method: String,
        /// Optional named parameters.
        params: Option<Map<String, Value>>,
    },
}

fn rpc_integer(number: &Number) -> Option<Number> {
    let integer = integer(number)?;
    i64::try_from(integer)
        .map(Number::from)
        .or_else(|_| u64::try_from(integer).map(Number::from))
        .ok()
}

/// Normalize an integral decimal exactly, with a magnitude bounded by `u64`.
///
/// Shared by protocol IDs and generated codecs; target types apply their own
/// narrower bounds. No intermediate floating-point conversion is permitted.
pub(crate) fn integer(number: &Number) -> Option<i128> {
    let text = number.to_string();
    let (negative, text) = text
        .strip_prefix('-')
        .map_or((false, text.as_str()), |text| (true, text));
    let (mantissa, exponent) = text.split_once(['e', 'E']).unwrap_or((text, "0"));
    let fraction = mantissa
        .split_once('.')
        .map_or(0, |(_, fraction)| fraction.len());
    let mut digits: Vec<_> = mantissa
        .bytes()
        .filter(|byte| *byte != b'.')
        .skip_while(|byte| *byte == b'0')
        .collect();
    if digits.is_empty() {
        return Some(0);
    }
    let exponent = exponent.parse::<i64>().unwrap_or_else(|_| {
        if exponent.starts_with('-') {
            i64::MIN
        } else {
            i64::MAX
        }
    });
    let shift = exponent.saturating_sub(i64::try_from(fraction).ok()?);
    if shift < 0 {
        let removed = usize::try_from(shift.unsigned_abs()).ok()?;
        let retained = digits.len().checked_sub(removed)?;
        if digits.get(retained..)?.iter().any(|byte| *byte != b'0') {
            return None;
        }
        digits.truncate(retained);
    } else {
        let added = usize::try_from(shift).ok()?;
        if added > 20_usize.saturating_sub(digits.len()) {
            return None;
        }
        digits.resize(digits.len() + added, b'0');
    }
    if digits.len() > 20 {
        return None;
    }
    let magnitude = digits.into_iter().try_fold(0_u64, |value, byte| {
        value.checked_mul(10)?.checked_add(u64::from(byte - b'0'))
    })?;
    let value = i128::from(magnitude);
    Some(if negative { -value } else { value })
}

fn object(value: Value, reason: &'static str) -> Result<Map<String, Value>, WireError> {
    match value {
        Value::Object(value) => Ok(value),
        _ => Err(WireError::InvalidEnvelope(reason)),
    }
}

/// Decode exactly one bounded UTF-8 MCP JSON-RPC 2.0 message.
///
/// The bound includes surrounding JSON whitespace. Batches, concatenated JSON,
/// a JSON BOM, invalid ID types, null/non-object results or parameters, and
/// conflicting reserved fields are rejected. Integer IDs and codes must fit
/// `i64`/`u64`; integral decimal/exponent spellings are normalized exactly.
/// Result numbers retain arbitrary precision. Nesting is limited to 128
/// containers. Unknown extension fields are not method-schema validated.
pub fn decode_json(body: &[u8], max_body_bytes: usize) -> Result<RpcMessage, WireError> {
    if body.len() > max_body_bytes {
        return Err(WireError::LimitExceeded {
            limit: Limit::Body,
            maximum: max_body_bytes,
        });
    }
    let text = std::str::from_utf8(body).map_err(|_| WireError::InvalidUtf8)?;
    let value = parse_value(text).map_err(|error: serde_json::Error| WireError::InvalidJson {
        line: error.line(),
        column: error.column(),
    })?;
    let mut envelope = object(
        value,
        "expected one object; JSON-RPC batches are not supported",
    )?;
    if envelope.remove("jsonrpc") != Some(Value::String("2.0".into())) {
        return Err(WireError::InvalidEnvelope(
            "jsonrpc must be the string \"2.0\"",
        ));
    }
    let id = envelope.remove("id").map(RpcId::decode).transpose()?;
    if let Some(method) = envelope.remove("method") {
        if envelope.contains_key("result") || envelope.contains_key("error") {
            return Err(WireError::InvalidEnvelope(
                "requests and notifications must not include result or error",
            ));
        }
        let Value::String(method) = method else {
            return Err(WireError::InvalidEnvelope("method must be a string"));
        };
        let params = envelope
            .remove("params")
            .map(|value| object(value, "params must be an object when present"))
            .transpose()?;
        return Ok(match id {
            Some(id) => RpcMessage::Request { id, method, params },
            None => RpcMessage::Notification { method, params },
        });
    }
    if envelope.contains_key("params") {
        return Err(WireError::InvalidEnvelope(
            "params requires a request or notification method",
        ));
    }
    match (envelope.remove("result"), envelope.remove("error")) {
        (Some(result), None) => {
            let id = id.ok_or(WireError::InvalidEnvelope("success response requires id"))?;
            Ok(RpcMessage::Success {
                id,
                result: object(result, "MCP result must be an object")?,
            })
        }
        (None, Some(error)) => {
            let mut error = object(error, "error must be an object")?;
            let code = match error.remove("code") {
                Some(Value::Number(code)) => {
                    rpc_integer(&code).ok_or(WireError::InvalidEnvelope(
                        "error.code must be an integer within i64/u64 range",
                    ))?
                }
                _ => {
                    return Err(WireError::InvalidEnvelope(
                        "error.code must be an integer encoded within i64/u64 range",
                    ));
                }
            };
            let message = match error.remove("message") {
                Some(Value::String(message)) => message,
                _ => return Err(WireError::InvalidEnvelope("error.message must be a string")),
            };
            Ok(RpcMessage::Error {
                id,
                error: RpcError {
                    code,
                    message,
                    data: error.remove("data"),
                },
            })
        }
        _ => Err(WireError::InvalidEnvelope(
            "response must include exactly one of result or error",
        )),
    }
}

/// Correlates a single outstanding request, without swallowing other messages.
#[derive(Debug)]
pub struct ResponseMatcher {
    expected: Option<RpcId>,
    matched: bool,
}

impl ResponseMatcher {
    /// Create a matcher; `None` describes an unsolicited server-message stream.
    #[must_use]
    pub fn new(expected: Option<RpcId>) -> Self {
        Self {
            expected,
            matched: false,
        }
    }

    /// Observe a message, returning `true` only for the matching success or error.
    ///
    /// Requests and notifications return `false` and remain available to the
    /// caller. A mismatched, duplicate, stray, or ID-less response is an error.
    pub fn observe(&mut self, message: &RpcMessage) -> Result<bool, WireError> {
        let id = match message {
            RpcMessage::Success { id, .. } => Some(id),
            RpcMessage::Error { id, .. } => id.as_ref(),
            RpcMessage::Request { .. } | RpcMessage::Notification { .. } => return Ok(false),
        };
        let expected = self
            .expected
            .as_ref()
            .filter(|_| !self.matched)
            .ok_or(WireError::UnexpectedResponse)?;
        let id = id.ok_or(WireError::UncorrelatedResponse)?;
        if id != expected {
            return Err(WireError::MismatchedResponseId);
        }
        self.matched = true;
        Ok(true)
    }

    /// Check whether EOF occurred before an expected reply.
    ///
    /// For SSE, call [`SseDecoder::finish`] first. A matcher without an
    /// outstanding request can finish successfully without any response.
    pub fn finish(&self) -> Result<(), WireError> {
        if self.expected.is_some() && !self.matched {
            Err(WireError::MissingResponse)
        } else {
            Ok(())
        }
    }
}

/// Independent byte budgets for an incremental SSE decoder.
///
/// No allocation is made from these numbers at construction. Retained content
/// is bounded by `max_event_bytes + 3 * max_buffer_bytes`: event data plus one
/// physical line, event type, and persistent ID. Allocators may round capacity
/// upwards; JSON parsing adds a bounded-size tree with a bounded recursion depth.
/// Returned messages belong to the caller and are never queued by the decoder.
/// Zero limits are valid and admit only input that uses none of that resource.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub struct SseLimits {
    /// Maximum bytes in a physical line, excluding CR/LF and the initial BOM.
    ///
    /// This applies to all fields, comments, and unknown fields, even before a
    /// line terminator arrives.
    pub max_buffer_bytes: usize,
    /// Maximum raw bytes in nonblank lines of one event block, with line endings.
    ///
    /// Comments and unknown fields count. The blank delimiter and initial BOM
    /// count only against the body limit. Reset only on a blank line, including
    /// for blocks with no data. Joined JSON data cannot exceed this limit.
    pub max_event_bytes: usize,
    /// Maximum raw bytes consumed over the entire stream, including BOM,
    /// blank lines, comments, ignored fields, and both bytes of CRLF.
    ///
    /// This is a work budget as well as a size budget: many tiny comments cannot
    /// bypass it. Unconsumed bytes after a returned event do not count.
    pub max_body_bytes: usize,
}

/// One blank-line-terminated SSE event containing at least one `data` field.
#[derive(Debug, PartialEq)]
pub struct SseEvent {
    /// Decoded JSON-RPC payload, or `None` for an empty priming event.
    pub message: Option<RpcMessage>,
    /// SSE event type, defaulting to `"message"` as specified by SSE.
    pub event_type: String,
    /// Persistent event ID, including the empty string when explicitly reset.
    pub id: String,
    /// Most recently parsed retry interval in milliseconds, if any.
    pub retry_ms: Option<u64>,
}

#[derive(Debug, Eq, PartialEq)]
enum State {
    Active,
    Finished,
    Failed,
}

/// Incremental strict-UTF-8 SSE framing and JSON-RPC decoding.
///
/// Supports LF, CR, split CRLF, one leading UTF-8 BOM, field values with one
/// optional leading space, comments, and unknown fields. A BOM anywhere else is
/// ordinary field/data content, not stripped. Unlike a browser's replacement
/// decoding, malformed UTF-8 is rejected to preserve MCP's UTF-8 requirement.
///
/// A failure poisons the decoder; no malformed input can be recovered by feeding
/// further bytes. Metadata-only blocks update accessors without producing an
/// event. No reconnect policy is implemented.
#[derive(Debug)]
pub struct SseDecoder {
    limits: SseLimits,
    line: Vec<u8>,
    data: String,
    has_data: bool,
    event_type: String,
    event_id: String,
    retry_ms: Option<u64>,
    body_bytes: usize,
    event_bytes: usize,
    at_start: bool,
    bom_bytes: u8,
    skip_lf: bool,
    cr_was_nonblank: bool,
    state: State,
}

impl SseDecoder {
    /// Construct an empty decoder with explicit byte budgets.
    #[must_use]
    pub fn new(limits: SseLimits) -> Self {
        Self {
            limits,
            line: Vec::new(),
            data: String::new(),
            has_data: false,
            event_type: String::new(),
            event_id: String::new(),
            retry_ms: None,
            body_bytes: 0,
            event_bytes: 0,
            at_start: true,
            bom_bytes: 0,
            skip_lf: false,
            cr_was_nonblank: false,
            state: State::Active,
        }
    }

    /// Consume input until one event is dispatched or the chunk runs out.
    ///
    /// Update the same slice in a loop until it is exhausted. `None` means all
    /// supplied bytes were consumed, not EOF. Bytes after a dispatched event are
    /// left untouched, even when malformed or over budget. A terminal CR can
    /// dispatch immediately; its optional following LF remains for the next call.
    pub fn next_event(&mut self, input: &mut &[u8]) -> Result<Option<SseEvent>, WireError> {
        self.ensure_active()?;
        let result = self.consume(input);
        if result.is_err() {
            self.state = State::Failed;
        }
        result
    }

    /// The latest valid `id` field, including metadata-only and empty events.
    ///
    /// Updated when its line terminates, possibly before event dispatch. IDs with
    /// NUL are ignored as required by SSE. This is metadata, not a reconnect API.
    #[must_use]
    pub fn event_id(&self) -> &str {
        &self.event_id
    }

    /// The latest valid retry interval. Invalid non-digit fields are ignored.
    ///
    /// An all-digit value too large for `u64` produces [`WireError::RetryOverflow`]
    /// instead of silently substituting a shorter reconnect delay.
    #[must_use]
    pub fn retry_ms(&self) -> Option<u64> {
        self.retry_ms
    }

    /// Verify clean EOF without dispatching a pending event.
    ///
    /// Any nonempty unterminated block, including a comment/metadata-only block,
    /// is truncated. An empty stream, only BOM, or a completed blank delimiter
    /// is clean framing; use [`ResponseMatcher::finish`] to require a reply.
    /// CR is a complete terminator without a following LF. Calling `finish`
    /// again after success is harmless, but feeding more input is an error.
    pub fn finish(&mut self) -> Result<(), WireError> {
        match self.state {
            State::Finished => return Ok(()),
            State::Failed => return Err(WireError::DecoderFailed),
            State::Active => {}
        }
        if self.bom_bytes != 0 || !self.line.is_empty() || self.event_bytes != 0 {
            self.state = State::Failed;
            return Err(WireError::TruncatedSse);
        }
        self.state = State::Finished;
        Ok(())
    }

    fn ensure_active(&self) -> Result<(), WireError> {
        match self.state {
            State::Active => Ok(()),
            State::Finished => Err(WireError::DecoderFinished),
            State::Failed => Err(WireError::DecoderFailed),
        }
    }

    fn consume(&mut self, input: &mut &[u8]) -> Result<Option<SseEvent>, WireError> {
        while let Some((&byte, rest)) = input.split_first() {
            increment(
                &mut self.body_bytes,
                self.limits.max_body_bytes,
                Limit::Body,
            )?;
            *input = rest;
            if self.at_start {
                const BOM: [u8; 3] = [0xef, 0xbb, 0xbf];
                if BOM.get(usize::from(self.bom_bytes)) == Some(&byte) {
                    self.bom_bytes += 1;
                    if self.bom_bytes == 3 {
                        self.bom_bytes = 0;
                        self.at_start = false;
                    }
                    continue;
                }
                self.at_start = false;
                for &prefix in BOM.iter().take(usize::from(self.bom_bytes)) {
                    self.push_byte(prefix)?;
                }
                self.bom_bytes = 0;
            }
            if self.skip_lf {
                self.skip_lf = false;
                if byte == b'\n' {
                    if self.cr_was_nonblank {
                        self.count_event_byte()?;
                    }
                    continue;
                }
            }
            if byte == b'\r' || byte == b'\n' {
                let nonblank = !self.line.is_empty();
                if nonblank {
                    self.count_event_byte()?;
                }
                self.skip_lf = byte == b'\r';
                self.cr_was_nonblank = nonblank;
                if let Some(event) = self.end_line()? {
                    return Ok(Some(event));
                }
            } else {
                self.push_byte(byte)?;
            }
        }
        Ok(None)
    }

    fn count_event_byte(&mut self) -> Result<(), WireError> {
        increment(
            &mut self.event_bytes,
            self.limits.max_event_bytes,
            Limit::Event,
        )
    }

    fn push_byte(&mut self, byte: u8) -> Result<(), WireError> {
        if self.line.len() >= self.limits.max_buffer_bytes {
            return Err(WireError::LimitExceeded {
                limit: Limit::Buffer,
                maximum: self.limits.max_buffer_bytes,
            });
        }
        self.count_event_byte()?;
        self.line.push(byte);
        Ok(())
    }

    fn end_line(&mut self) -> Result<Option<SseEvent>, WireError> {
        if self.line.is_empty() {
            self.event_bytes = 0;
            let event_type = std::mem::take(&mut self.event_type);
            if !self.has_data {
                return Ok(None);
            }
            self.has_data = false;
            let data = std::mem::take(&mut self.data);
            let message = if data.is_empty() {
                None
            } else {
                Some(decode_json(data.as_bytes(), self.limits.max_event_bytes)?)
            };
            return Ok(Some(SseEvent {
                message,
                event_type: if event_type.is_empty() {
                    "message".into()
                } else {
                    event_type
                },
                id: self.event_id.clone(),
                retry_ms: self.retry_ms,
            }));
        }

        let line = std::str::from_utf8(&self.line).map_err(|_| WireError::InvalidUtf8)?;
        let (field, value) = line.split_once(':').unwrap_or((line, ""));
        let value = value.strip_prefix(' ').unwrap_or(value);
        match field {
            "data" => {
                if self.has_data {
                    self.data.push('\n');
                }
                self.data.push_str(value);
                self.has_data = true;
            }
            "event" => {
                self.event_type.clear();
                self.event_type.push_str(value);
            }
            "id" if !value.contains('\0') => {
                self.event_id.clear();
                self.event_id.push_str(value);
            }
            "retry" if !value.is_empty() && value.bytes().all(|byte| byte.is_ascii_digit()) => {
                self.retry_ms = Some(value.parse().map_err(|_| WireError::RetryOverflow)?);
            }
            _ => {}
        }
        self.line.clear();
        Ok(None)
    }
}

fn increment(count: &mut usize, maximum: usize, limit: Limit) -> Result<(), WireError> {
    if *count >= maximum {
        return Err(WireError::LimitExceeded { limit, maximum });
    }
    *count += 1;
    Ok(())
}

/// Parse real JSON containers separately from arbitrary-precision number tokens.
///
/// Borrowed raw children avoid repeatedly copying nested subtrees. Using
/// deserialize_any for containers would confuse a real object named
/// `$serde_json::private::Number` with serde_json's internal number transport.
pub(crate) fn parse_value(text: &str) -> Result<Value, serde_json::Error> {
    let raw: &RawValue = serde_json::from_str(text)?;
    parse_raw(raw, 0)
}

fn parse_raw(raw: &RawValue, depth: usize) -> Result<Value, serde_json::Error> {
    struct JsonVisitor {
        depth: usize,
    }

    impl<'de> Visitor<'de> for JsonVisitor {
        type Value = Value;

        fn expecting(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
            f.write_str("JSON without duplicate object keys")
        }

        fn visit_seq<A: SeqAccess<'de>>(self, mut seq: A) -> Result<Value, A::Error> {
            let mut values = Vec::new();
            while let Some(raw) = seq.next_element::<&RawValue>()? {
                values.push(parse_raw(raw, self.depth + 1).map_err(de::Error::custom)?);
            }
            Ok(Value::Array(values))
        }

        fn visit_map<A: MapAccess<'de>>(self, mut map: A) -> Result<Value, A::Error> {
            let mut values = Map::new();
            while let Some((key, raw)) = map.next_entry::<String, &RawValue>()? {
                if values.contains_key(&key) {
                    return Err(de::Error::custom("duplicate JSON object key"));
                }
                values.insert(
                    key,
                    parse_raw(raw, self.depth + 1).map_err(de::Error::custom)?,
                );
            }
            Ok(Value::Object(values))
        }
    }

    let text = raw.get().trim();
    let container = matches!(text.as_bytes().first(), Some(b'{' | b'['));
    if container && depth >= 128 {
        return Err(de::Error::custom(
            "JSON nesting limit exceeded (128 containers)",
        ));
    }
    let mut decoder = serde_json::Deserializer::from_str(text);
    match text.as_bytes().first() {
        Some(b'{') => decoder.deserialize_map(JsonVisitor { depth }),
        Some(b'[') => decoder.deserialize_seq(JsonVisitor { depth }),
        _ => serde_json::from_str(text),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    const RESPONSE: &str = r#"{"jsonrpc":"2.0","id":0,"result":{}}"#;
    const PING: &str = r#"{"jsonrpc":"2.0","id":17,"method":"ping"}"#;
    const NOTICE: &str =
        r#"{"jsonrpc":"2.0","method":"notifications/progress","params":{"progress":1}}"#;

    #[test]
    fn duplicate_wire_keys_are_rejected_before_value_erases_them() {
        for text in [
            r#"{"jsonrpc":"2.0","id":0,"id":0,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":{"structuredContent":{"x":1,"x":2}}}"#,
        ] {
            assert!(matches!(
                decode_json(text.as_bytes(), 1024),
                Err(WireError::InvalidJson { .. })
            ));
        }
    }

    #[test]
    fn result_numbers_and_private_marker_objects_remain_lossless() {
        let RpcMessage::Success { result, .. } = rpc(
            r#"{"jsonrpc":"2.0","id":1,"result":{"_meta":{"large":18446744073709551617,"tiny":1e-999,"literal":{"$serde_json::private::Number":"7"},"raw":{"$serde_json::private::RawValue":"123"}}}}"#,
        ) else {
            panic!()
        };
        let result = Value::Object(result);
        assert_eq!(
            result.pointer("/_meta/large").unwrap().to_string(),
            "18446744073709551617"
        );
        assert_eq!(result.pointer("/_meta/tiny").unwrap().to_string(), "1e-999");
        assert_eq!(
            result
                .pointer("/_meta/literal/$serde_json::private::Number")
                .and_then(Value::as_str),
            Some("7")
        );
        assert_eq!(
            result
                .pointer("/_meta/raw/$serde_json::private::RawValue")
                .and_then(Value::as_str),
            Some("123")
        );
        assert!(
            parse_value(
                r#"{"$serde_json::private::Number":"1","$serde_json::private::Number":"2"}"#
            )
            .is_err()
        );
        assert!(
            parse_value(" \n {\"$serde_json::private::Number\":\"7\"} \t")
                .unwrap()
                .is_object()
        );
    }

    #[test]
    fn raw_json_nesting_is_bounded_at_128_containers() {
        for depth in [127, 128] {
            let text = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            assert!(parse_value(&text).is_ok(), "depth {depth}");
        }
        for depth in [129, 1024] {
            let text = format!("{}0{}", "[".repeat(depth), "]".repeat(depth));
            assert!(parse_value(&text).is_err(), "depth {depth}");
        }
    }

    #[test]
    fn integer_normalization_is_exact_without_float_rounding() {
        for (text, expected) in [
            ("-128.0", -128),
            ("1e2", 100),
            ("-0", 0),
            ("0e999999999999999999999999", 0),
            ("1.2300e2", 123),
            ("0.000001e6", 1),
            ("18446744073709551615.000", i128::from(u64::MAX)),
            ("-9223372036854775808.0", i128::from(i64::MIN)),
        ] {
            assert_eq!(
                integer(&text.parse::<Number>().unwrap()),
                Some(expected),
                "{text}"
            );
        }
        for text in [
            "1.0000000000000000000001",
            "1e-99999999999999999999",
            "1e99999999999999999999",
            "18446744073709551616.0",
            "18446744073709551615.1",
            "100000000000000000000000000000000001e-17",
        ] {
            assert_eq!(integer(&text.parse::<Number>().unwrap()), None, "{text}");
        }
    }

    #[test]
    fn rpc_ids_and_codes_accept_integral_decimal_and_exponent_forms() {
        for id in ["1", "1.0", "1e0", "0.01e2"] {
            let message = rpc(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#));
            let mut matcher = ResponseMatcher::new(Some(RpcId::Number(1.into())));
            assert!(matcher.observe(&message).unwrap());
        }
        let mut matcher = ResponseMatcher::new(Some(RpcId::Number(0.into())));
        assert!(
            matcher
                .observe(&rpc(r#"{"jsonrpc":"2.0","id":-0.0,"result":{}}"#))
                .unwrap()
        );
        assert!(matches!(
            rpc(r#"{"jsonrpc":"2.0","id":1.0,"error":{"code":-3.2602e4,"message":"invalid"}}"#),
            RpcMessage::Error { error: RpcError { code, .. }, .. } if code == Number::from(-32602)
        ));
        assert!(decode_json(br#"{"jsonrpc":"2.0","id":1.5,"result":{}}"#, 1024).is_err());
        let mut matcher = ResponseMatcher::new(Some(RpcId::Number(1.into())));
        assert_eq!(
            matcher.observe(&rpc(r#"{"jsonrpc":"2.0","id":"1","result":{}}"#)),
            Err(WireError::MismatchedResponseId)
        );
    }

    fn limits() -> SseLimits {
        SseLimits {
            max_buffer_bytes: 1024,
            max_event_bytes: 4096,
            max_body_bytes: 16384,
        }
    }

    fn rpc(text: &str) -> RpcMessage {
        decode_json(text.as_bytes(), 4096).expect("valid fixture JSON-RPC")
    }

    fn framed(text: &str) -> String {
        format!("data: {text}\n\n")
    }

    fn drain(decoder: &mut SseDecoder, mut input: &[u8]) -> Result<Vec<SseEvent>, WireError> {
        let mut events = Vec::new();
        while let Some(event) = decoder.next_event(&mut input)? {
            events.push(event);
        }
        assert!(input.is_empty());
        Ok(events)
    }

    fn fragmented(bytes: &[u8], chunk_size: usize) -> Result<Vec<SseEvent>, WireError> {
        let mut decoder = SseDecoder::new(limits());
        let mut events = Vec::new();
        for chunk in bytes.chunks(chunk_size) {
            events.extend(drain(&mut decoder, chunk)?);
        }
        decoder.finish()?;
        Ok(events)
    }

    fn limit_error(limit: Limit, maximum: usize) -> WireError {
        WireError::LimitExceeded { limit, maximum }
    }

    #[test]
    fn json_classifies_success_request_and_notification() {
        assert_eq!(
            rpc(RESPONSE),
            RpcMessage::Success {
                id: RpcId::Number(0.into()),
                result: Map::new(),
            }
        );
        assert_eq!(
            rpc(PING),
            RpcMessage::Request {
                id: RpcId::Number(17.into()),
                method: "ping".into(),
                params: None,
            }
        );
        let RpcMessage::Notification { method, params } = rpc(NOTICE) else {
            panic!("expected notification");
        };
        assert_eq!(method, "notifications/progress");
        assert_eq!(
            params.and_then(|p| p.get("progress").cloned()),
            Some(json!(1))
        );
        assert!(matches!(
            rpc(r#"{"jsonrpc":"2.0","id":"","method":"unknown","params":{}}"#),
            RpcMessage::Request { id: RpcId::String(id), params: Some(_), .. } if id.is_empty()
        ));
        assert!(matches!(
            rpc(r#"{"jsonrpc":"2.0","method":"","params":{}}"#),
            RpcMessage::Notification { method, .. } if method.is_empty()
        ));
    }

    #[test]
    fn json_preserves_rpc_error_payload_and_optional_data() {
        let text = r#"{"jsonrpc":"2.0","id":"a","error":{"code":-32601,"message":"unsupported","data":{"detail":42},"extension":true}}"#;
        assert_eq!(
            rpc(text),
            RpcMessage::Error {
                id: Some(RpcId::String("a".into())),
                error: RpcError {
                    code: (-32601).into(),
                    message: "unsupported".into(),
                    data: Some(json!({"detail": 42})),
                },
            }
        );
        for (data, expected) in [("", None), (r#","data":null"#, Some(Value::Null))] {
            let message = rpc(&format!(
                r#"{{"jsonrpc":"2.0","id":0,"error":{{"code":0,"message":""{data}}}}}"#
            ));
            let RpcMessage::Error { error, .. } = message else {
                panic!("expected RPC error");
            };
            assert_eq!(error.data, expected);
        }
    }

    #[test]
    fn json_accepts_idless_protocol_errors_but_they_cannot_match() {
        let message = rpc(r#"{"jsonrpc":"2.0","error":{"code":-32700,"message":"parse error"}}"#);
        assert!(matches!(message, RpcMessage::Error { id: None, .. }));
        let mut matcher = ResponseMatcher::new(Some(RpcId::Number(0.into())));
        assert_eq!(
            matcher.observe(&message),
            Err(WireError::UncorrelatedResponse)
        );
        assert_eq!(matcher.finish(), Err(WireError::MissingResponse));
    }

    #[test]
    fn json_preserves_extensions_in_payload_without_interpreting_tool_is_error() {
        let message = rpc(
            r#"{"jsonrpc":"2.0","id":0,"result":{"isError":true,"_meta":{"private":true}},"extension":42}"#,
        );
        let RpcMessage::Success { result, .. } = message else {
            panic!("isError is still a successful JSON-RPC response");
        };
        assert_eq!(result.get("isError"), Some(&Value::Bool(true)));
        assert_eq!(result.get("_meta"), Some(&json!({"private":true})));
        assert!(matches!(
            rpc(r#"{"jsonrpc":"2.0","method":"extra","extension":true}"#),
            RpcMessage::Notification { .. }
        ));
    }

    #[test]
    fn json_rejects_malformed_envelopes() {
        let cases = [
            "null",
            "0",
            "true",
            r#""string""#,
            "[]",
            r#"[{"jsonrpc":"2.0","id":0,"result":{}}]"#,
            "{}",
            r#"{"id":0,"result":{}}"#,
            r#"{"jsonrpc":"1.0","id":0,"result":{}}"#,
            r#"{"jsonrpc":2.0,"id":0,"result":{}}"#,
            r#"{"jsonrpc":null,"id":0,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":null,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":false,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":[],"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":{},"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":0.5,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1.1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":1e-1,"result":{}}"#,
            r#"{"jsonrpc":"2.0","id":18446744073709551616,"result":{}}"#,
            r#"{"jsonrpc":"2.0","result":{}}"#,
            r#"{"jsonrpc":"2.0","id":0}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":{},"error":{}}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":{},"error":null}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":null}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":[]}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":"ok"}"#,
            r#"{"jsonrpc":"2.0","id":0,"result":{},"params":null}"#,
            r#"{"jsonrpc":"2.0","params":{}}"#,
            r#"{"jsonrpc":"2.0","method":null}"#,
            r#"{"jsonrpc":"2.0","method":2}"#,
            r#"{"jsonrpc":"2.0","method":[]}"#,
            r#"{"jsonrpc":"2.0","method":"ping","id":null}"#,
            r#"{"jsonrpc":"2.0","method":"ping","id":true}"#,
            r#"{"jsonrpc":"2.0","method":"ping","result":{}}"#,
            r#"{"jsonrpc":"2.0","method":"ping","error":{}}"#,
            r#"{"jsonrpc":"2.0","method":"ping","id":0,"result":{}}"#,
            r#"{"jsonrpc":"2.0","method":"ping","id":0,"params":[]}"#,
            r#"{"jsonrpc":"2.0","method":"ping","params":null}"#,
            r#"{"jsonrpc":"2.0","method":"ping","params":0}"#,
            r#"{"jsonrpc":"2.0","method":"ping","params":false}"#,
            r#"{"jsonrpc":"2.0","method":"ping","params":"secret"}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":null}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":[]}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-1}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"message":"bad"}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":"-1","message":"bad"}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":false,"message":"bad"}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":1.5,"message":"bad"}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-1,"message":null}}"#,
            r#"{"jsonrpc":"2.0","id":0,"error":{"code":-1,"message":{}}}"#,
            r#"{"jsonrpc":"2.0","id":null,"error":{"code":-1,"message":"bad"}}"#,
        ];
        for text in cases {
            assert!(
                matches!(
                    decode_json(text.as_bytes(), 4096),
                    Err(WireError::InvalidEnvelope(_))
                ),
                "accepted malformed fixture: {text}"
            );
        }
    }

    #[test]
    fn json_rejects_invalid_utf8_syntax_trailing_values_and_bom() {
        assert_eq!(decode_json(&[0xff], 10), Err(WireError::InvalidUtf8));
        for text in ["", " ", "{", "{\"jsonrpc\":", "NaN", "{}", "\u{feff}{}"] {
            let result = decode_json(text.as_bytes(), 4096);
            assert!(result.is_err());
        }
        for text in [
            format!("{RESPONSE}{RESPONSE}"),
            format!("{RESPONSE} x"),
            format!("{RESPONSE} null"),
        ] {
            assert!(matches!(
                decode_json(text.as_bytes(), 4096),
                Err(WireError::InvalidJson { .. })
            ));
        }
        assert_eq!(rpc(&format!(" \r\n{RESPONSE}\t ")), rpc(RESPONSE));
    }

    #[test]
    fn json_body_limit_is_exact_and_includes_whitespace_utf8_bytes() {
        let text = r#" {"jsonrpc":"2.0","id":"é","result":{}} "#;
        assert!(decode_json(text.as_bytes(), text.len()).is_ok());
        assert_eq!(
            decode_json(text.as_bytes(), text.len() - 1),
            Err(limit_error(Limit::Body, text.len() - 1))
        );
        assert_eq!(decode_json(b"x", 0), Err(limit_error(Limit::Body, 0)));
    }

    #[test]
    fn json_default_recursion_limit_is_not_disabled() {
        let deep = format!(
            r#"{{"jsonrpc":"2.0","id":0,"result":{{"nested":{}0{}}}}}"#,
            "[".repeat(200),
            "]".repeat(200)
        );
        assert!(matches!(
            decode_json(deep.as_bytes(), 4096),
            Err(WireError::InvalidJson { .. })
        ));
    }

    #[test]
    fn matcher_preserves_integer_id_precision_and_string_type() {
        for id in [
            "0",
            "-1",
            "9223372036854775807",
            "-9223372036854775808",
            "18446744073709551615",
            "\"\"",
            "\"0\"",
        ] {
            let message = rpc(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#));
            let RpcMessage::Success { id, .. } = &message else {
                panic!("expected response");
            };
            let mut matcher = ResponseMatcher::new(Some(id.clone()));
            assert_eq!(matcher.observe(&message), Ok(true));
            assert_eq!(matcher.finish(), Ok(()));
            assert_eq!(
                matcher.observe(&message),
                Err(WireError::UnexpectedResponse)
            );
        }
        for id in ["1", "-1", "\"0\"", "9007199254740993"] {
            let mut matcher = ResponseMatcher::new(Some(RpcId::Number(0.into())));
            let message = rpc(&format!(r#"{{"jsonrpc":"2.0","id":{id},"result":{{}}}}"#));
            assert_eq!(
                matcher.observe(&message),
                Err(WireError::MismatchedResponseId)
            );
            assert_eq!(matcher.finish(), Err(WireError::MissingResponse));
        }
        let mut matcher =
            ResponseMatcher::new(Some(RpcId::Number(9_007_199_254_740_992_u64.into())));
        let message = rpc(r#"{"jsonrpc":"2.0","id":9007199254740993,"result":{}}"#);
        assert_eq!(
            matcher.observe(&message),
            Err(WireError::MismatchedResponseId)
        );
    }

    #[test]
    fn matcher_ignores_requests_even_with_same_id_and_matches_rpc_errors() {
        let mut matcher = ResponseMatcher::new(Some(RpcId::Number(17.into())));
        assert_eq!(matcher.observe(&rpc(PING)), Ok(false));
        assert_eq!(matcher.observe(&rpc(NOTICE)), Ok(false));
        assert_eq!(matcher.finish(), Err(WireError::MissingResponse));
        let error = rpc(r#"{"jsonrpc":"2.0","id":17,"error":{"code":-1,"message":"failed"}}"#);
        assert_eq!(matcher.observe(&error), Ok(true));
        assert_eq!(matcher.finish(), Ok(()));
    }

    #[test]
    fn matcher_rejects_stray_response_on_unsolicited_stream() {
        let mut matcher = ResponseMatcher::new(None);
        assert_eq!(matcher.observe(&rpc(PING)), Ok(false));
        assert_eq!(
            matcher.observe(&rpc(RESPONSE)),
            Err(WireError::UnexpectedResponse)
        );
        assert_eq!(matcher.finish(), Ok(()));
    }

    #[test]
    fn sse_all_line_endings_and_every_chunk_size() {
        for ending in ["\n", "\r", "\r\n"] {
            let stream = format!(": comment{ending}data: {RESPONSE}{ending}{ending}");
            for size in 1..=stream.len() {
                let events = fragmented(stream.as_bytes(), size).expect("valid fragmented SSE");
                assert_eq!(events.len(), 1);
                assert_eq!(
                    events.first().and_then(|e| e.message.as_ref()),
                    Some(&rpc(RESPONSE))
                );
            }
        }
    }

    #[test]
    fn sse_every_two_part_split_including_bom_crlf_and_utf8() {
        let stream = "\u{feff}id: café\r\n: ignored\rdata: {\"jsonrpc\":\"2.0\",\ndata: \"id\":\"🦀\",\"result\":{}}\r\n\r\n";
        let expected = fragmented(stream.as_bytes(), stream.len()).expect("whole stream");
        for split in 0..=stream.len() {
            let (first, second) = stream.as_bytes().split_at(split);
            let mut decoder = SseDecoder::new(limits());
            let mut actual = drain(&mut decoder, first).expect("first chunk");
            actual.extend(drain(&mut decoder, second).expect("second chunk"));
            decoder.finish().expect("clean finish");
            assert_eq!(actual, expected, "split at byte {split}");
        }
        assert_eq!(
            fragmented(stream.as_bytes(), 1).expect("each byte"),
            expected
        );
    }

    #[test]
    fn sse_multiline_json_and_unknown_fields_follow_standard_field_rules() {
        let stream = concat!(
            ": heartbeat\n",
            "unknown: ignored\n",
            "unknown-without-colon\n",
            "DATA: ignored-case-sensitive\n",
            "event: custom\n",
            "data: {\"jsonrpc\":\"2.0\",\n",
            "data:\"id\":0,\n",
            "data:  \"result\":{}}\n",
            "\n",
        );
        let events = fragmented(stream.as_bytes(), 1).expect("multiline event");
        let event = events.first().expect("one event");
        assert_eq!(event.message, Some(rpc(RESPONSE)));
        assert_eq!(event.event_type, "custom");
        let stream =
            "data: {\"jsonrpc\":\"2.0\",\"id\":0,\"result\":{\"s\":\"hel\ndata: lo\"}}\n\n";
        assert!(
            matches!(
                fragmented(stream.as_bytes(), 1),
                Err(WireError::InvalidJson { .. })
            ),
            "data lines must be joined with a newline, not concatenated"
        );
    }

    #[test]
    fn sse_empty_data_priming_and_metadata_only_blocks_are_not_json() {
        let stream = "id: first\nretry: 20\n\ndata:\n\nid\nretry: invalid\ndata\n\n";
        let mut decoder = SseDecoder::new(limits());
        let events = drain(&mut decoder, stream.as_bytes()).expect("priming");
        assert_eq!(events.len(), 2);
        let first = events.first().expect("first event");
        assert_eq!(first.message, None);
        assert_eq!(first.id, "first");
        assert_eq!(first.retry_ms, Some(20));
        let last = events.last().expect("last event");
        assert_eq!(last.message, None);
        assert_eq!(last.id, "");
        assert_eq!(last.retry_ms, Some(20));
        assert_eq!(decoder.event_id(), "");
        assert_eq!(decoder.retry_ms(), Some(20));
        decoder.finish().expect("clean finish");
    }

    #[test]
    fn sse_only_exactly_empty_priming_data_is_skipped() {
        for stream in ["data:\ndata:\n\n", "data:  \n\n", "data: \t\n\n"] {
            assert!(matches!(
                fragmented(stream.as_bytes(), 1),
                Err(WireError::InvalidJson { .. })
            ));
        }
    }

    #[test]
    fn sse_metadata_persists_and_event_type_resets() {
        let stream = format!(
            "id: one\nid:  two\nretry: 0007\nevent: custom\ndata:\n\nid: bad\0id\ndata: {RESPONSE}\n\nid:\nevent: custom\nevent:\ndata:\n\n"
        );
        let events = fragmented(stream.as_bytes(), 1).expect("metadata events");
        let mut events = events.into_iter();
        let first = events.next().expect("first");
        assert_eq!(first.id, " two");
        assert_eq!(first.event_type, "custom");
        assert_eq!(first.retry_ms, Some(7));
        let second = events.next().expect("second");
        assert_eq!(second.id, " two");
        assert_eq!(second.event_type, "message");
        assert_eq!(second.retry_ms, Some(7));
        let third = events.next().expect("third");
        assert_eq!(third.id, "");
        assert_eq!(third.event_type, "message");
    }

    #[test]
    fn sse_retry_validation_and_overflow() {
        let mut decoder = SseDecoder::new(limits());
        drain(&mut decoder, b"retry: 17\n\n").expect("initial retry");
        for text in [
            "retry:\n\n",
            "retry: -1\n\n",
            "retry: +2\n\n",
            "retry: 2.5\n\n",
            "retry:  2\n\n",
            "retry: 2 \n\n",
            "retry: ٢\n\n",
        ] {
            drain(&mut decoder, text.as_bytes()).expect("invalid retry ignored by SSE");
            assert_eq!(decoder.retry_ms(), Some(17));
        }
        drain(&mut decoder, b"retry: 18446744073709551615\n\n").expect("maximum u64 retry");
        assert_eq!(decoder.retry_ms(), Some(u64::MAX));
        assert_eq!(
            drain(&mut decoder, b"retry: 18446744073709551616\n\n"),
            Err(WireError::RetryOverflow)
        );
    }

    #[test]
    fn sse_bom_only_at_absolute_stream_start() {
        let mut decoder = SseDecoder::new(limits());
        assert!(
            drain(&mut decoder, "\u{feff}".as_bytes())
                .expect("only BOM")
                .is_empty()
        );
        decoder.finish().expect("BOM alone has no pending event");
        let stream = format!("\u{feff}data: {RESPONSE}\n\n");
        assert_eq!(
            fragmented(stream.as_bytes(), 1).expect("leading BOM").len(),
            1
        );
        for prefix in ["\n\u{feff}", "\u{feff}\u{feff}"] {
            let stream = format!("{prefix}data: {RESPONSE}\n\n");
            assert!(
                fragmented(stream.as_bytes(), 1)
                    .expect("unknown BOM-prefixed field")
                    .is_empty()
            );
        }
        let stream = format!("data: \u{feff}{RESPONSE}\n\n");
        assert!(matches!(
            fragmented(stream.as_bytes(), 1),
            Err(WireError::InvalidJson { .. })
        ));
        let text = r#"{"jsonrpc":"2.0","id":0,"result":{"bom":"﻿"}}"#;
        let events = fragmented(framed(text).as_bytes(), 1).expect("BOM as string data");
        assert_eq!(
            events.first().and_then(|e| e.message.as_ref()),
            Some(&rpc(text))
        );
    }

    #[test]
    fn sse_rejects_invalid_utf8_in_data_comments_and_unknown_fields() {
        for bytes in [
            b"data: \xff\n\n".as_slice(),
            b":\xff\n\n".as_slice(),
            b"unknown:\xff\n\n".as_slice(),
            b"\xefx\n\n".as_slice(),
            b"\xef\xbbx\n\n".as_slice(),
            b":\xc0\xaf\n\n".as_slice(),
            b":\xed\xa0\x80\n\n".as_slice(),
        ] {
            assert_eq!(fragmented(bytes, 1), Err(WireError::InvalidUtf8));
        }
    }

    #[test]
    fn sse_multiple_messages_before_response_and_unused_trailing_bytes() {
        let prefix = format!(
            "id: primed\ndata:\n\n{}{}{}{}",
            framed(NOTICE),
            framed(PING),
            framed(r#"{"jsonrpc":"2.0","id":"unknown","method":"unsupported","params":{"x":1}}"#),
            framed(RESPONSE),
        );
        let mut bytes = prefix.as_bytes().to_vec();
        bytes.extend_from_slice(b"data: unfinished\xff");
        bytes.extend(std::iter::repeat_n(b'x', 10000));
        let mut input = bytes.as_slice();
        let mut decoder = SseDecoder::new(SseLimits {
            max_body_bytes: prefix.len(),
            ..limits()
        });
        let mut matcher = ResponseMatcher::new(Some(RpcId::Number(0.into())));
        let mut messages = Vec::new();
        while let Some(event) = decoder.next_event(&mut input).expect("next event") {
            if let Some(message) = event.message {
                let matched = matcher.observe(&message).expect("valid response ID");
                messages.push(message);
                if matched {
                    break;
                }
            }
        }
        assert_eq!(messages.len(), 4);
        assert_eq!(messages.last(), Some(&rpc(RESPONSE)));
        assert!(input.starts_with(b"data: unfinished\xff"));
        assert_eq!(decoder.body_bytes, prefix.len());
        assert_eq!(matcher.finish(), Ok(()));
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_dispatches_at_cr_without_waiting_for_optional_lf_or_eof() {
        let stream = format!("data: {RESPONSE}\r\n\r\nbad");
        let mut input = stream.as_bytes();
        let mut decoder = SseDecoder::new(limits());
        let event = decoder
            .next_event(&mut input)
            .expect("valid")
            .expect("dispatched at CR");
        assert_eq!(event.message, Some(rpc(RESPONSE)));
        assert_eq!(input, b"\nbad");
        assert_eq!(decoder.finish(), Ok(()));
    }

    #[test]
    fn sse_eof_never_dispatches_an_unterminated_event() {
        for stream in [
            "data: ".to_string(),
            format!("data: {RESPONSE}"),
            format!("data: {RESPONSE}\n"),
            format!("data: {RESPONSE}\r\n"),
            format!("data: {RESPONSE}\r"),
            ": comment\n".to_string(),
            "id: only\n".to_string(),
        ] {
            let mut decoder = SseDecoder::new(limits());
            assert!(
                drain(&mut decoder, stream.as_bytes())
                    .expect("partial input")
                    .is_empty()
            );
            assert_eq!(decoder.finish(), Err(WireError::TruncatedSse), "{stream:?}");
        }
        for bytes in [
            b"\xef".as_slice(),
            b"\xef\xbb".as_slice(),
            b"data:\xc3".as_slice(),
        ] {
            let mut decoder = SseDecoder::new(limits());
            assert!(
                drain(&mut decoder, bytes)
                    .expect("partial UTF-8")
                    .is_empty()
            );
            assert_eq!(decoder.finish(), Err(WireError::TruncatedSse));
        }
    }

    #[test]
    fn sse_clean_eof_is_distinct_from_no_matching_response() {
        for stream in [
            "".to_string(),
            "\n\r\n\r".into(),
            ": comment\n\n".into(),
            framed(NOTICE),
            "data:\n\n".into(),
        ] {
            let mut decoder = SseDecoder::new(limits());
            let mut matcher = ResponseMatcher::new(Some(RpcId::Number(0.into())));
            for event in drain(&mut decoder, stream.as_bytes()).expect("valid") {
                if let Some(message) = event.message {
                    assert!(!matcher.observe(&message).expect("no response"));
                }
            }
            assert_eq!(decoder.finish(), Ok(()));
            assert_eq!(matcher.finish(), Err(WireError::MissingResponse));
        }
    }

    #[test]
    fn sse_line_buffer_limit_exact_and_over_before_any_separator() {
        let mut decoder = SseDecoder::new(SseLimits {
            max_buffer_bytes: 4,
            ..limits()
        });
        drain(&mut decoder, b":abc").expect("exactly four bytes without a separator");
        assert_eq!(decoder.line.len(), 4);
        drain(&mut decoder, b"\n\n").expect("separators do not count as line bytes");
        assert_eq!(decoder.finish(), Ok(()));
        let mut decoder = SseDecoder::new(SseLimits {
            max_buffer_bytes: 4,
            ..limits()
        });
        drain(&mut decoder, b":abc").expect("at limit");
        assert_eq!(
            drain(&mut decoder, b"d"),
            Err(limit_error(Limit::Buffer, 4))
        );
        assert_eq!(decoder.line.len(), 4);
    }

    #[test]
    fn sse_line_buffer_limit_counts_utf8_bytes_not_characters() {
        let mut decoder = SseDecoder::new(SseLimits {
            max_buffer_bytes: 3,
            ..limits()
        });
        drain(&mut decoder, ":é\n\n".as_bytes()).expect("three bytes");
        let mut decoder = SseDecoder::new(SseLimits {
            max_buffer_bytes: 2,
            ..limits()
        });
        assert_eq!(
            drain(&mut decoder, ":é".as_bytes()),
            Err(limit_error(Limit::Buffer, 2))
        );
    }

    #[test]
    fn sse_event_limit_counts_ignored_fields_and_split_crlf_exactly() {
        for (bytes, maximum) in [
            (b":a\nx\n\n".as_slice(), 5),
            (b":a\rx\r\r".as_slice(), 5),
            (b":a\r\nx\r\n\r\n".as_slice(), 7),
        ] {
            let mut decoder = SseDecoder::new(SseLimits {
                max_event_bytes: maximum,
                ..limits()
            });
            for byte in bytes {
                drain(&mut decoder, std::slice::from_ref(byte)).expect("exact event bound");
            }
            decoder.finish().expect("complete block");
            let mut decoder = SseDecoder::new(SseLimits {
                max_event_bytes: maximum - 1,
                ..limits()
            });
            assert_eq!(
                drain(&mut decoder, bytes),
                Err(limit_error(Limit::Event, maximum - 1))
            );
        }
        let mut decoder = SseDecoder::new(SseLimits {
            max_event_bytes: 4,
            ..limits()
        });
        drain(&mut decoder, b"xxxx").expect("event bytes counted before a separator");
        assert_eq!(drain(&mut decoder, b"x"), Err(limit_error(Limit::Event, 4)));
    }

    #[test]
    fn sse_json_event_limit_exact_and_over_and_resets_between_blocks() {
        let stream = framed(RESPONSE);
        let maximum = stream.len() - 1;
        let mut decoder = SseDecoder::new(SseLimits {
            max_event_bytes: maximum,
            ..limits()
        });
        assert_eq!(
            drain(&mut decoder, stream.as_bytes())
                .expect("exact bound")
                .len(),
            1
        );
        assert_eq!(
            drain(&mut decoder, stream.as_bytes())
                .expect("event budget resets")
                .len(),
            1
        );
        let mut decoder = SseDecoder::new(SseLimits {
            max_event_bytes: maximum - 1,
            ..limits()
        });
        assert_eq!(
            drain(&mut decoder, stream.as_bytes()),
            Err(limit_error(Limit::Event, maximum - 1))
        );
    }

    #[test]
    fn sse_body_limit_counts_bom_comments_blank_lines_and_crlf() {
        let bytes = b"\xef\xbb\xbf:\r\n\r\n\n";
        let mut decoder = SseDecoder::new(SseLimits {
            max_body_bytes: 9,
            ..limits()
        });
        drain(&mut decoder, bytes).expect("exact body bound");
        assert_eq!(decoder.body_bytes, 9);
        decoder.finish().expect("clean finish");
        let mut decoder = SseDecoder::new(SseLimits {
            max_body_bytes: 8,
            ..limits()
        });
        assert_eq!(drain(&mut decoder, bytes), Err(limit_error(Limit::Body, 8)));
        assert_eq!(decoder.body_bytes, 8);
    }

    #[test]
    fn sse_repeated_tiny_comments_cannot_bypass_event_or_body_budgets() {
        let mut decoder = SseDecoder::new(SseLimits {
            max_event_bytes: 6,
            ..limits()
        });
        for _ in 0..3 {
            drain(&mut decoder, b":\n").expect("bounded comment");
        }
        assert_eq!(decoder.event_bytes, 6);
        assert!(decoder.line.is_empty() && decoder.data.is_empty());
        assert_eq!(drain(&mut decoder, b":"), Err(limit_error(Limit::Event, 6)));
        let mut decoder = SseDecoder::new(SseLimits {
            max_body_bytes: 9,
            ..limits()
        });
        for _ in 0..3 {
            drain(&mut decoder, b":\n\n").expect("comment-only block");
        }
        assert_eq!(decoder.event_bytes, 0);
        assert_eq!(decoder.body_bytes, 9);
        assert_eq!(drain(&mut decoder, b":"), Err(limit_error(Limit::Body, 9)));
    }

    #[test]
    fn sse_limits_zero_and_usize_max_do_not_allocate_or_overflow() {
        let mut decoder = SseDecoder::new(SseLimits {
            max_buffer_bytes: 0,
            max_event_bytes: 0,
            max_body_bytes: 0,
        });
        assert_eq!(decoder.line.capacity(), 0);
        drain(&mut decoder, b"").expect("empty input");
        assert_eq!(drain(&mut decoder, b"\n"), Err(limit_error(Limit::Body, 0)));
        let mut decoder = SseDecoder::new(SseLimits {
            max_buffer_bytes: 0,
            max_event_bytes: 0,
            max_body_bytes: 8,
        });
        drain(&mut decoder, b"\xef\xbb\xbf\r\n\n").expect("no line or event bytes");
        assert_eq!(
            drain(&mut decoder, b"x"),
            Err(limit_error(Limit::Buffer, 0))
        );
        let decoder = SseDecoder::new(SseLimits {
            max_buffer_bytes: usize::MAX,
            max_event_bytes: usize::MAX,
            max_body_bytes: usize::MAX,
        });
        assert_eq!(decoder.line.capacity(), 0);
        assert_eq!(decoder.data.capacity(), 0);
        let mut count = usize::MAX;
        assert_eq!(
            increment(&mut count, usize::MAX, Limit::Body),
            Err(limit_error(Limit::Body, usize::MAX))
        );
    }

    #[test]
    fn sse_retains_bounded_buffers_across_many_metadata_and_data_lines() {
        let bounds = SseLimits {
            max_buffer_bytes: 16,
            max_event_bytes: 256,
            max_body_bytes: 2048,
        };
        let mut decoder = SseDecoder::new(bounds);
        let bytes = b"id: 0123456789\n\nevent: abc\nid: 9876543210\ndata:\n";
        for byte in bytes.iter().chain(b"data:\n".iter().cycle().take(1000)) {
            if drain(&mut decoder, std::slice::from_ref(byte)).is_err() {
                break;
            }
            assert!(decoder.line.len() <= bounds.max_buffer_bytes);
            assert!(decoder.event_type.len() <= bounds.max_buffer_bytes);
            assert!(decoder.event_id.len() <= bounds.max_buffer_bytes);
            assert!(decoder.data.len() <= bounds.max_event_bytes);
            assert!(decoder.event_bytes <= bounds.max_event_bytes);
            assert!(decoder.body_bytes <= bounds.max_body_bytes);
        }
        assert_eq!(decoder.state, State::Failed);
        assert!(decoder.data.len() <= bounds.max_event_bytes);
    }

    #[test]
    fn sse_failures_poison_decoder_and_finish_is_terminal() {
        let mut decoder = SseDecoder::new(limits());
        assert!(drain(&mut decoder, b"data: not-json\n\n").is_err());
        assert_eq!(
            drain(&mut decoder, framed(RESPONSE).as_bytes()),
            Err(WireError::DecoderFailed)
        );
        assert_eq!(decoder.finish(), Err(WireError::DecoderFailed));
        let mut decoder = SseDecoder::new(limits());
        assert_eq!(decoder.finish(), Ok(()));
        assert_eq!(decoder.finish(), Ok(()));
        assert_eq!(drain(&mut decoder, b""), Err(WireError::DecoderFinished));
    }

    #[test]
    fn diagnostics_do_not_echo_untrusted_json_method_id_or_sse_data() {
        for text in [
            r#"{"jsonrpc":"private-secret","id":0,"result":{}}"#,
            r#"{"jsonrpc":"2.0","method":{"private-secret":true}}"#,
            r#"{"jsonrpc":"2.0","id":{"private-secret":true},"result":{}}"#,
            r#"{"private-secret": invalid}"#,
        ] {
            let error = decode_json(text.as_bytes(), 4096).expect_err("malformed");
            assert!(!error.to_string().contains("private-secret"));
            assert!(!format!("{error:?}").contains("private-secret"));
        }
        let error = fragmented(b"data: private-secret\n\n", 1).expect_err("not JSON");
        assert!(!error.to_string().contains("private-secret"));
        assert!(!format!("{error:?}").contains("private-secret"));
        let mut matcher = ResponseMatcher::new(Some(RpcId::Number(0.into())));
        let error = matcher
            .observe(&rpc(
                r#"{"jsonrpc":"2.0","id":"private-secret","result":{}}"#,
            ))
            .expect_err("mismatch");
        assert!(!error.to_string().contains("private-secret"));
    }

    #[test]
    fn arbitrary_short_bytes_do_not_panic_or_exceed_budgets() {
        let bounds = SseLimits {
            max_buffer_bytes: 16,
            max_event_bytes: 24,
            max_body_bytes: 48,
        };
        for first in 0..=u8::MAX {
            for second in [0, b'\n', b'\r', b':', 0xbb, 0xbf, 0xff] {
                let bytes = [first, second, b'\n', b'\n'];
                let mut decoder = SseDecoder::new(bounds);
                for byte in &bytes {
                    if drain(&mut decoder, std::slice::from_ref(byte)).is_err() {
                        break;
                    }
                }
                let _ = decoder.finish();
                let _ = decode_json(&bytes, 48);
                assert!(decoder.line.len() <= bounds.max_buffer_bytes);
                assert!(decoder.data.len() <= bounds.max_event_bytes);
                assert!(decoder.body_bytes <= bounds.max_body_bytes);
            }
        }
    }
}
