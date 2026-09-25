//! Native, asynchronous HTTP transport for MCP sessions.
//!
//! Requires a Tokio runtime with I/O and time enabled. Use
//! [`NativeTransport::new`](crate::native::NativeTransport::new)
//! with a positive maximum chunk size (for example,
//! 16 KiB, or a smaller runtime response limit). The bound limits each copied
//! chunk, not reqwest's internal network buffers or total response size.
//! [`crate::runtime::Session`] applies total response/event limits and deadlines.
//!
//! HTTP/1.1 is intentional: no pooled connections, redirects, retries, cookies,
//! ambient proxy configuration, or automatic decompression. HTTPS uses rustls
//! with its normal certificate verification. No credentials are stored by this
//! adapter; only the supplied request carries them.

use std::{
    fmt,
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::Duration,
};

use bytes::Bytes;
use http::{HeaderMap, HeaderName, HeaderValue, Method};
use tokio::time::{Instant, sleep_until};

use crate::runtime::{Body, Error, Request, Response, Transport, validate_endpoint};

/// A native transport; clones share client configuration, not MCP session state.
#[derive(Clone)]
pub struct NativeTransport {
    client: reqwest::Client,
    max_chunk_bytes: usize,
}

impl fmt::Debug for NativeTransport {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeTransport")
            .field("max_chunk_bytes", &self.max_chunk_bytes)
            .finish_non_exhaustive()
    }
}

impl NativeTransport {
    /// Build a credential-free client, rejecting a zero chunk bound.
    ///
    /// Both sending and streaming must run inside [`Transport::within`] to get a
    /// total deadline. [`crate::runtime::Session`] already does this.
    pub fn new(max_chunk_bytes: usize) -> Result<Self, Error> {
        if max_chunk_bytes == 0 {
            return Err(Error::Transport(
                "native response chunk bound must be positive".into(),
            ));
        }
        let client = reqwest::Client::builder()
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .no_proxy()
            .referer(false)
            .http1_only()
            // Also avoid lower-level replay on a stale pooled connection.
            .pool_max_idle_per_host(0)
            .no_gzip()
            .no_brotli()
            .no_deflate()
            .no_zstd()
            .build()
            .map_err(http_error)?;
        Ok(Self {
            client,
            max_chunk_bytes,
        })
    }
}

/// An owned response stream. Dropping it releases the pending HTTP body.
pub struct NativeBody {
    response: reqwest::Response,
    pending: Bytes,
    max_chunk_bytes: usize,
}

impl fmt::Debug for NativeBody {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("NativeBody")
            .field("buffered_bytes", &self.pending.len())
            .field("max_chunk_bytes", &self.max_chunk_bytes)
            .finish_non_exhaustive()
    }
}

impl Body for NativeBody {
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        if self.pending.is_empty() {
            let Some(chunk) = self.response.chunk().await.map_err(http_error)? else {
                return Ok(None);
            };
            self.pending = chunk;
        }
        let count = self.pending.len().min(self.max_chunk_bytes);
        Ok(Some(self.pending.split_to(count).to_vec()))
    }
}

impl Transport for NativeTransport {
    type Body = NativeBody;

    async fn send(&self, request: Request) -> Result<Response<Self::Body>, Error> {
        validate_endpoint(&request.endpoint)?;
        let method = Method::from_bytes(request.method.as_bytes())
            .map_err(|_| Error::Transport("invalid outgoing HTTP method".into()))?;
        let mut headers = HeaderMap::new();
        for (name, value) in request.headers {
            let name = HeaderName::from_bytes(name.as_bytes())
                .map_err(|_| Error::Transport("invalid outgoing HTTP header name".into()))?;
            let mut value = HeaderValue::from_str(&value)
                .map_err(|_| Error::Transport("invalid outgoing HTTP header value".into()))?;
            value.set_sensitive(true);
            headers.append(name, value);
        }
        let response = self
            .client
            .request(method, &request.endpoint)
            .headers(headers)
            .body(request.body)
            .send()
            .await
            .map_err(http_error)?;
        let status = response.status().as_u16();
        let headers = response
            .headers()
            .iter()
            .map(|(name, value)| {
                let value = value
                    .to_str()
                    .map_err(|_| Error::Protocol("non-ASCII HTTP response header".into()))?;
                Ok((name.as_str().to_owned(), value.to_owned()))
            })
            .collect::<Result<_, Error>>()?;
        Ok(Response {
            status,
            headers,
            body: NativeBody {
                response,
                pending: Bytes::new(),
                max_chunk_bytes: self.max_chunk_bytes,
            },
        })
    }

    async fn within<T>(
        &self,
        timeout: Duration,
        future: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        let deadline = Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| Error::Transport("native request timeout is out of range".into()))?;
        let mut timer = pin!(sleep_until(deadline));
        let mut future = pin!(future);
        poll_fn(|cx| {
            // Check wall-clock time too: a self-waking future may prevent Tokio's
            // timer driver from advancing on a busy current-thread executor.
            if timer.as_mut().poll(cx).is_ready() || Instant::now() >= deadline {
                return Poll::Ready(Err(timeout_error()));
            }
            let result = future.as_mut().poll(cx);
            if Instant::now() >= deadline {
                Poll::Ready(Err(timeout_error()))
            } else {
                result
            }
        })
        .await
    }
}

fn timeout_error() -> Error {
    Error::Transport("total request timeout exceeded; not replayed".into())
}

fn http_error(error: reqwest::Error) -> Error {
    // Never format the error or its source: even without a URL those may carry
    // untrusted HTTP data. Keep only safe, useful categories.
    let category = if error.is_timeout() {
        "HTTP request timed out; not replayed"
    } else if error.is_connect() {
        "HTTP connection failed; not replayed"
    } else if error.is_builder() {
        "invalid HTTP request or client configuration"
    } else if error.is_body() || error.is_decode() {
        "HTTP response body interrupted or malformed; not replayed"
    } else {
        "HTTP request failed or response malformed; not replayed"
    };
    Error::Transport(category.into())
}
