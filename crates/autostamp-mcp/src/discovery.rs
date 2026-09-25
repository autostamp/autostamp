//! Bounded tools-only discovery over the shared MCP session implementation.

use std::collections::BTreeSet;

use serde_json::{Value, json};

use crate::runtime::{Error, Limits, Session, Transport};

/// Limits applied to the whole catalog, in addition to per-request limits.
#[derive(Clone, Copy, Debug)]
pub struct DiscoveryLimits {
    /// Maximum number of `tools/list` requests.
    pub max_pages: usize,
    /// Maximum number of distinct discovered tools.
    pub max_tools: usize,
    /// Maximum serialized size of the returned `{"tools":[...]}` catalog.
    pub max_catalog_bytes: usize,
}

impl Default for DiscoveryLimits {
    fn default() -> Self {
        Self {
            max_pages: 100,
            max_tools: 10_000,
            max_catalog_bytes: 16 * 1024 * 1024,
        }
    }
}

impl DiscoveryLimits {
    /// Reject limits that cannot permit even an empty catalog.
    pub fn validate(self) -> Result<(), Error> {
        if self.max_pages == 0 || self.max_tools == 0 || self.max_catalog_bytes < 12 {
            return Err(Error::Protocol(
                "discovery requires positive page/tool limits and at least 12 catalog bytes".into(),
            ));
        }
        Ok(())
    }
}

/// Initialize a session, discover every tool, and close the session when supported.
///
/// Never invokes a tool. Returned tool definitions retain their complete JSON;
/// protocol pagination metadata is not part of the generated catalog.
/// Empty catalogs are valid and generate clients without tool functions.
pub async fn discover<T: Transport>(
    transport: T,
    endpoint: String,
    bearer: Option<String>,
    request_limits: Limits,
    limits: DiscoveryLimits,
) -> Result<Value, Error> {
    limits.validate()?;
    let mut session = Session::new(transport, endpoint, bearer, request_limits)?;
    let result = async {
        session.initialize().await?;
        collect(&mut session, limits).await
    }
    .await;
    let cleanup = session.close().await;
    match (result, cleanup) {
        (Ok(catalog), Ok(())) => Ok(catalog),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) => Err(Error::Transport(format!(
            "discovery failed: {error}; session cleanup also failed: {cleanup}"
        ))),
    }
}

async fn collect<T: Transport>(
    session: &mut Session<T>,
    limits: DiscoveryLimits,
) -> Result<Value, Error> {
    let mut tools = Vec::new();
    let mut names = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    let mut cursor = None;
    let mut catalog_bytes = 12_usize;

    for _ in 0..limits.max_pages {
        let params = match cursor.take() {
            Some(cursor) => json!({ "cursor": cursor }),
            None => json!({}),
        };
        let page = session.request("tools/list", params).await?;
        let object = page
            .as_object()
            .ok_or_else(|| Error::Protocol("tools/list result must be an object".into()))?;
        let page_tools = object
            .get("tools")
            .and_then(Value::as_array)
            .ok_or_else(|| Error::Protocol("tools/list result requires a tools array".into()))?;

        for tool in page_tools {
            let name = tool
                .get("name")
                .and_then(Value::as_str)
                .filter(|name| !name.is_empty())
                .ok_or_else(|| Error::Protocol("listed tool requires a nonempty name".into()))?;
            if !names.insert(name.to_owned()) {
                return Err(Error::Protocol(
                    "tools/list returned a duplicate tool name".into(),
                ));
            }
            if tools.len() >= limits.max_tools {
                return Err(Error::Protocol("discovery tool limit exceeded".into()));
            }
            let bytes = serde_json::to_vec(tool)
                .map_err(|_| Error::Protocol("tool definition could not be serialized".into()))?;
            catalog_bytes = catalog_bytes
                .checked_add(bytes.len())
                .and_then(|total| total.checked_add(usize::from(!tools.is_empty())))
                .filter(|total| *total <= limits.max_catalog_bytes)
                .ok_or_else(|| Error::Protocol("discovery catalog byte limit exceeded".into()))?;
            tools.push(tool.clone());
        }

        match object.get("nextCursor") {
            None => {
                tools.sort_by(|a, b| {
                    a.get("name")
                        .and_then(Value::as_str)
                        .cmp(&b.get("name").and_then(Value::as_str))
                });
                return Ok(json!({ "tools": tools }));
            }
            Some(Value::String(next)) => {
                if !cursors.insert(next.clone()) {
                    return Err(Error::Protocol(
                        "tools/list returned a repeated pagination cursor".into(),
                    ));
                }
                cursor = Some(next.clone());
            }
            Some(_) => {
                return Err(Error::Protocol(
                    "tools/list nextCursor must be a string when present".into(),
                ));
            }
        }
    }
    Err(Error::Protocol("discovery page limit exceeded".into()))
}

#[cfg(test)]
mod tests {
    use std::cell::RefCell;
    use std::collections::VecDeque;
    use std::future::Future;
    use std::rc::Rc;
    use std::time::Duration;

    use super::*;
    use crate::runtime::{Body, Request, Response};

    #[derive(Debug)]
    struct TestBody(Option<Vec<u8>>);

    impl Body for TestBody {
        async fn next(&mut self) -> Result<Option<Vec<u8>>, Error> {
            Ok(self.0.take())
        }
    }

    #[derive(Clone, Debug)]
    struct TestTransport {
        responses: Rc<RefCell<VecDeque<Response<TestBody>>>>,
        requests: Rc<RefCell<Vec<Request>>>,
    }

    impl Transport for TestTransport {
        type Body = TestBody;

        async fn send(&self, request: Request) -> Result<Response<TestBody>, Error> {
            self.requests.borrow_mut().push(request);
            self.responses
                .borrow_mut()
                .pop_front()
                .ok_or_else(|| Error::Transport("unexpected request".into()))
        }

        async fn within<T>(
            &self,
            _timeout: Duration,
            future: impl Future<Output = Result<T, Error>>,
        ) -> Result<T, Error> {
            future.await
        }
    }

    fn response(id: usize, result: Value) -> Response<TestBody> {
        Response {
            status: 200,
            headers: vec![("content-type".into(), "application/json".into())],
            body: TestBody(Some(
                json!({"jsonrpc":"2.0","id":id,"result":result})
                    .to_string()
                    .into_bytes(),
            )),
        }
    }

    fn fixture(pages: Vec<Value>, session: bool) -> TestTransport {
        let mut initialize = response(
            1,
            json!({
                "protocolVersion": "2025-11-25",
                "capabilities": {"tools": {}},
                "serverInfo": {"name":"fixture","version":"1"}
            }),
        );
        if session {
            initialize
                .headers
                .push(("mcp-session-id".into(), "fixture-session".into()));
        }
        let mut responses = VecDeque::from([
            initialize,
            Response {
                status: 202,
                headers: vec![],
                body: TestBody(None),
            },
        ]);
        responses.extend(
            pages
                .into_iter()
                .enumerate()
                .map(|(index, page)| response(index + 2, page)),
        );
        if session {
            responses.push_back(Response {
                status: 204,
                headers: vec![],
                body: TestBody(None),
            });
        }
        TestTransport {
            responses: Rc::new(RefCell::new(responses)),
            requests: Rc::new(RefCell::new(Vec::new())),
        }
    }

    async fn run(transport: TestTransport, limits: DiscoveryLimits) -> Result<Value, Error> {
        discover(
            transport,
            "http://127.0.0.1/mcp".into(),
            None,
            Limits::default(),
            limits,
        )
        .await
    }

    fn tool(name: &str) -> Value {
        json!({
            "name": name,
            "inputSchema": {"type":"object","additionalProperties":false},
            "description":"Preserve me"
        })
    }

    #[tokio::test(flavor = "current_thread")]
    async fn follows_all_pages_preserves_definitions_sorts_and_closes() {
        let transport = fixture(
            vec![
                json!({"tools":[tool("z")],"nextCursor":"opaque"}),
                json!({"tools":[tool("a")]}),
            ],
            true,
        );
        let result = run(transport.clone(), DiscoveryLimits::default())
            .await
            .unwrap();
        assert_eq!(result, json!({"tools":[tool("a"),tool("z")]}));
        let requests = transport.requests.borrow();
        let methods: Vec<_> = requests.iter().map(|request| &request.method).collect();
        assert_eq!(methods, ["POST", "POST", "POST", "POST", "DELETE"]);
        let second_list: Value = serde_json::from_slice(&requests.get(3).unwrap().body).unwrap();
        assert_eq!(second_list.get("method"), Some(&json!("tools/list")));
        assert_eq!(
            second_list.pointer("/params/cursor"),
            Some(&json!("opaque"))
        );
        for request in requests.iter().filter(|request| request.method == "POST") {
            let body: Value = serde_json::from_slice(&request.body).unwrap();
            assert_ne!(body.get("method"), Some(&json!("tools/call")));
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_cursor_is_opaque_and_not_a_terminal_sentinel() {
        let transport = fixture(
            vec![
                json!({"tools":[],"nextCursor":""}),
                json!({"tools":[tool("next")]}),
            ],
            false,
        );
        let result = run(transport.clone(), DiscoveryLimits::default())
            .await
            .unwrap();
        assert_eq!(result, json!({"tools":[tool("next")]}));
        let request: Value =
            serde_json::from_slice(&transport.requests.borrow().get(3).unwrap().body).unwrap();
        assert_eq!(request.pointer("/params/cursor"), Some(&json!("")));
    }

    #[tokio::test(flavor = "current_thread")]
    async fn empty_catalog_is_distinct_from_malformed_response() {
        assert_eq!(
            run(
                fixture(vec![json!({"tools":[]})], false),
                DiscoveryLimits::default()
            )
            .await
            .unwrap(),
            json!({"tools":[]})
        );
        for page in [json!({}), json!({"tools":null}), json!([])] {
            assert!(
                run(fixture(vec![page], false), DiscoveryLimits::default())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_duplicates_and_repeated_cursors() {
        for (pages, message) in [
            (
                vec![json!({"tools":[tool("x"),tool("x")]})],
                "duplicate tool",
            ),
            (
                vec![
                    json!({"tools":[tool("x")],"nextCursor":"next"}),
                    json!({"tools":[tool("x")]}),
                ],
                "duplicate tool",
            ),
            (
                vec![
                    json!({"tools":[],"nextCursor":"repeat"}),
                    json!({"tools":[],"nextCursor":"repeat"}),
                ],
                "repeated pagination",
            ),
        ] {
            let error = run(fixture(pages, false), DiscoveryLimits::default())
                .await
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn rejects_invalid_cursor_or_name() {
        for page in [
            json!({"tools":[],"nextCursor":null}),
            json!({"tools":[],"nextCursor":3}),
            json!({"tools":[{}]}),
            json!({"tools":[{"name":""}]}),
        ] {
            assert!(
                run(fixture(vec![page], false), DiscoveryLimits::default())
                    .await
                    .is_err()
            );
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn page_limit_is_exact_and_never_returns_partial_catalog() {
        let limits = DiscoveryLimits {
            max_pages: 1,
            ..DiscoveryLimits::default()
        };
        assert!(
            run(fixture(vec![json!({"tools":[tool("x")]})], false), limits)
                .await
                .is_ok()
        );
        let transport = fixture(vec![json!({"tools":[tool("x")],"nextCursor":"next"})], true);
        let error = run(transport.clone(), limits).await.unwrap_err();
        assert!(error.to_string().contains("page limit"));
        assert_eq!(transport.requests.borrow().last().unwrap().method, "DELETE");
    }

    #[tokio::test(flavor = "current_thread")]
    async fn tool_and_byte_limits_are_exact() {
        let page = json!({"tools":[tool("x"),tool("y")]});
        let byte_count = page.to_string().len();
        let limits = DiscoveryLimits {
            max_tools: 2,
            max_catalog_bytes: byte_count,
            ..DiscoveryLimits::default()
        };
        let result = run(fixture(vec![page.clone()], false), limits)
            .await
            .unwrap();
        assert_eq!(result.to_string().len(), byte_count);
        for (limits, message) in [
            (
                DiscoveryLimits {
                    max_tools: 1,
                    ..limits
                },
                "tool limit",
            ),
            (
                DiscoveryLimits {
                    max_catalog_bytes: byte_count - 1,
                    ..limits
                },
                "byte limit",
            ),
        ] {
            let error = run(fixture(vec![page.clone()], false), limits)
                .await
                .unwrap_err();
            assert!(error.to_string().contains(message), "{error}");
        }
    }

    #[tokio::test(flavor = "current_thread")]
    async fn validates_limits_before_network_access() {
        let transport = fixture(vec![], false);
        assert!(
            run(
                transport.clone(),
                DiscoveryLimits {
                    max_pages: 0,
                    ..DiscoveryLimits::default()
                }
            )
            .await
            .is_err()
        );
        assert!(transport.requests.borrow().is_empty());
    }
}
