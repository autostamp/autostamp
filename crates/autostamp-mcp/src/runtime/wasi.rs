//! WASI 0.2 adapter. Included only in generated clients, not the generator.

use std::{
    future::{Future, poll_fn},
    pin::pin,
    task::Poll,
    time::{Duration, Instant},
};

use serde_json::{Value, json};
use wstd::http::{
    Client,
    body::{
        Bytes,
        util::{BodyExt, combinators::UnsyncBoxBody},
    },
};

use crate::runtime::{self, Body, Error, Limits, Request, Response, Session, Transport};

struct WasiBody(UnsyncBoxBody<Bytes, wstd::http::Error>);

impl Body for WasiBody {
    async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
        loop {
            let Some(frame) = self.0.frame().await else {
                return Ok(None);
            };
            let frame = frame.map_err(|_| Error::Transport("HTTP response body interrupted".into()))?;
            if let Ok(bytes) = frame.into_data() {
                return Ok(Some(bytes.to_vec()));
            }
        }
    }
}

struct WasiTransport(Client);

impl Transport for WasiTransport {
    type Body = WasiBody;

    async fn send(&self, request: Request) -> Result<Response<WasiBody>, Error> {
        let mut builder = wstd::http::Request::builder()
            .method(request.method.as_str())
            .uri(request.endpoint.as_str());
        for (name, value) in request.headers {
            builder = builder.header(name, value);
        }
        let request = builder
            .body(request.body)
            .map_err(|_| Error::Transport("invalid outgoing HTTP request".into()))?;
        let response = self
            .0
            .send(request)
            .await
            .map_err(|_| Error::Transport("HTTP request failed; not replayed".into()))?;
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
            body: WasiBody(response.into_body().into_boxed_body()),
        })
    }

    async fn within<T>(
        &self,
        timeout: Duration,
        future: impl Future<Output = Result<T, Error>>,
    ) -> Result<T, Error> {
        let started = Instant::now();
        let timer = wstd::time::Timer::after(timeout.into());
        let mut timer = pin!(timer.wait());
        let mut future = pin!(future);
        poll_fn(|cx| {
            if started.elapsed() >= timeout || timer.as_mut().poll(cx).is_ready() {
                return Poll::Ready(Err(Error::Transport(
                    "total request timeout exceeded; not replayed".into(),
                )));
            }
            let result = future.as_mut().poll(cx);
            if started.elapsed() >= timeout {
                Poll::Ready(Err(Error::Transport(
                    "total request timeout exceeded; not replayed".into(),
                )))
            } else {
                result
            }
        })
        .await
    }
}

/// Invoke exactly once in a fresh initialized session, then release its ID.
pub(crate) fn call(
    endpoint: &str,
    bearer_secret: Option<&str>,
    limits: Limits,
    tool: &str,
    arguments: Value,
) -> Result<Value, Error> {
    limits.validate()?;
    let bearer = crate::credentials::load(bearer_secret)?;
    let timeout = Duration::from_millis(limits.request_timeout_ms);
    let mut client = Client::new();
    client.set_connect_timeout(timeout);
    client.set_first_byte_timeout(timeout);
    client.set_between_bytes_timeout(timeout);
    let mut session = Session::new(WasiTransport(client), endpoint.into(), bearer, limits)?;
    wstd::runtime::block_on(async {
        let result = async {
            session.initialize().await?;
            runtime::tool_result(
                session
                    .request("tools/call", json!({"name":tool,"arguments":arguments}))
                    .await?,
            )
        }
        .await;
        let closed = session.close().await;
        match (result, closed) {
            (Err(error), _) => Err(error),
            (Ok(_), Err(error)) => Err(error),
            (Ok(result), Ok(())) => Ok(result),
        }
    })
}
