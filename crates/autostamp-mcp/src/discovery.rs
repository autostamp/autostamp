//! Bounded, tools-only discovery using the official Rust MCP SDK.
//!
//! `rmcp` owns initialization, response correlation, server-request dispatch,
//! HTTP session headers, and shutdown. Local code only bounds I/O and catalogs
//! and preserves raw tool definitions that SDK typed models would normalize.

use std::{collections::BTreeSet, future::Future, sync::Arc, time::Duration};

use rmcp::{
    ClientLifecycleMode, ClientServiceExt,
    model::{
        ClientCapabilities, ClientConfig, ClientRequest, ClientResult, CustomResult, ErrorCode,
        ErrorData, Implementation, ListToolsRequest, PaginatedRequestParams, ProtocolVersion,
        ServerNotification, ServerRequest, ServerResult,
    },
    service::{NotificationContext, RequestContext, RoleClient, Service},
    transport::{
        common::client_side_sse::NeverRetry,
        streamable_http_client::{
            StreamableHttpClientTransport, StreamableHttpClientTransportConfig,
        },
    },
};
use serde_json::{Value, json};

use crate::{
    native::NativeTransport,
    runtime::{self, Error, Limits, Transport},
    sdk_http::{RAW_TOOLS_PAGE, SdkHttp},
};

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

#[derive(Debug)]
struct DiscoveryClient;

impl Service<RoleClient> for DiscoveryClient {
    async fn handle_request(
        &self,
        request: ServerRequest,
        _context: RequestContext<RoleClient>,
    ) -> Result<ClientResult, ErrorData> {
        if matches!(request, ServerRequest::PingRequest(_)) {
            Ok(ClientResult::empty(()))
        } else {
            Err(ErrorData::new(
                ErrorCode::METHOD_NOT_FOUND,
                "unsupported server request",
                None,
            ))
        }
    }

    async fn handle_notification(
        &self,
        _notification: ServerNotification,
        _context: NotificationContext<RoleClient>,
    ) -> Result<(), ErrorData> {
        Ok(())
    }

    fn get_info(&self) -> ClientConfig {
        ClientConfig::new(
            ClientCapabilities::default(),
            Implementation::new("autostamp-mcp", env!("CARGO_PKG_VERSION")),
        )
        .with_protocol_version(ProtocolVersion::V_2025_11_25)
    }
}

/// Initialize through `rmcp`, discover every tool, and close the HTTP session.
///
/// Never invokes a tool. Complete raw tool definitions are retained, while
/// pagination metadata is omitted. Empty catalogs generate no tool functions.
/// Requires an active Tokio runtime. Native discovery does not use the custom
/// [`runtime::Session`] needed by generated WASI clients.
pub async fn discover(
    transport: NativeTransport,
    endpoint: String,
    bearer: Option<String>,
    request_limits: Limits,
    limits: DiscoveryLimits,
) -> Result<Value, Error> {
    limits.validate()?;
    request_limits.validate()?;
    runtime::validate_endpoint(&endpoint)?;
    if bearer.as_ref().is_some_and(|value| {
        value.is_empty() || !value.bytes().all(|byte| matches!(byte, 0x21..=0x7e))
    }) {
        return Err(Error::Transport("invalid bearer credential".into()));
    }
    let deadline = Duration::from_millis(request_limits.request_timeout_ms);
    let adapter = SdkHttp::new(transport.clone(), request_limits);
    let mut config = StreamableHttpClientTransportConfig::with_uri(endpoint);
    config.auth_header = bearer;
    config.retry_config = Arc::new(NeverRetry::default());
    config.reinit_on_expired_session = false;
    config.max_concurrent_requests = 1;
    config.control_request_timeout = deadline;
    config.max_sse_event_size = request_limits.max_event_bytes;
    let sdk = StreamableHttpClientTransport::with_client(adapter.clone(), config);
    let initialized = transport
        .within(deadline, async {
            DiscoveryClient
                .serve_with_lifecycle(sdk, ClientLifecycleMode::Initialize)
                .await
                .map_err(|_| sdk_error(&adapter, "MCP initialization failed"))
        })
        .await;
    let mut client = match initialized {
        Ok(client) => {
            adapter.complete_bootstrap();
            client
        }
        Err(error) => {
            return with_cleanup(Err(error), adapter.cleanup_bootstrap().await);
        }
    };
    let result = async {
        let info = client
            .peer_info()
            .ok_or_else(|| Error::Protocol("missing server information".into()))?;
        if info.protocol_version != ProtocolVersion::V_2025_11_25 {
            return Err(Error::Protocol(
                "unsupported negotiated protocol version".into(),
            ));
        }
        if info.capabilities.tools.is_none() {
            return Err(Error::Protocol(
                "server must declare the tools capability".into(),
            ));
        }
        collect(
            |cursor| {
                let client = &client;
                let adapter = &adapter;
                let transport = &transport;
                async move {
                    transport
                        .within(deadline, async {
                            let request =
                                ClientRequest::ListToolsRequest(ListToolsRequest::with_param(
                                    PaginatedRequestParams::default().with_cursor(cursor),
                                ));
                            let result = client
                                .send_request(request)
                                .await
                                .map_err(|_| sdk_error(adapter, "MCP tools/list request failed"))?;
                            let ServerResult::CustomResult(CustomResult(value)) = result else {
                                return Err(Error::Protocol(
                                    "SDK adapter returned an unexpected tools result".into(),
                                ));
                            };
                            let raw = value
                                .get(RAW_TOOLS_PAGE)
                                .and_then(Value::as_str)
                                .ok_or_else(|| {
                                    Error::Protocol("SDK adapter omitted the raw tools page".into())
                                })?;
                            runtime::wire::parse_value(raw).map_err(|_| {
                                Error::Protocol(
                                    "SDK adapter returned invalid raw tools JSON".into(),
                                )
                            })
                        })
                        .await
                }
            },
            limits,
        )
        .await
    }
    .await;
    let result = with_cleanup(result, adapter.drain().await);
    let cleanup = match client.close_with_timeout(deadline).await {
        Ok(Some(_)) => adapter.error().map_or(Ok(()), Err),
        Ok(None) => Err(Error::Transport(
            "MCP session cleanup timeout exceeded".into(),
        )),
        Err(_) => Err(sdk_error(&adapter, "MCP session cleanup failed")),
    };
    with_cleanup(result, cleanup)
}

fn with_cleanup<T>(result: Result<T, Error>, cleanup: Result<(), Error>) -> Result<T, Error> {
    match (result, cleanup) {
        (Ok(catalog), Ok(())) => Ok(catalog),
        (Err(error), Ok(())) | (Ok(_), Err(error)) => Err(error),
        (Err(error), Err(cleanup)) if error == cleanup => Err(error),
        (Err(error), Err(cleanup)) => Err(Error::Transport(format!(
            "discovery failed: {error}; session cleanup also failed: {cleanup}"
        ))),
    }
}

fn sdk_error(adapter: &SdkHttp, fallback: &str) -> Error {
    adapter
        .error()
        .unwrap_or_else(|| Error::Transport(fallback.into()))
}

async fn collect<F, Fut>(mut list: F, limits: DiscoveryLimits) -> Result<Value, Error>
where
    F: FnMut(Option<String>) -> Fut,
    Fut: Future<Output = Result<Value, Error>>,
{
    limits.validate()?;
    let mut tools = Vec::new();
    let mut names = BTreeSet::new();
    let mut cursors = BTreeSet::new();
    let mut cursor = None;
    let mut catalog_bytes = 12_usize;
    for _ in 0..limits.max_pages {
        let page = list(cursor.take()).await?;
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
                return Ok(json!({"tools":tools}));
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
    use super::*;
    use std::{collections::VecDeque, future::ready};

    async fn pages(pages: Vec<Value>, limits: DiscoveryLimits) -> Result<Value, Error> {
        let mut pages: VecDeque<_> = pages.into();
        collect(
            |_| {
                ready(
                    pages
                        .pop_front()
                        .ok_or_else(|| Error::Transport("unexpected page request".into())),
                )
            },
            limits,
        )
        .await
    }

    fn tool(name: &str) -> Value {
        json!({"name":name,"description":"Preserve me","inputSchema":{"type":"object","additionalProperties":false}})
    }

    #[tokio::test]
    async fn follows_pages_preserves_definitions_and_sorts() {
        let page = pages(
            vec![
                json!({"tools":[tool("z")],"nextCursor":"opaque"}),
                json!({"tools":[tool("a")]}),
            ],
            DiscoveryLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(page, json!({"tools":[tool("a"),tool("z")]}));
    }

    #[tokio::test]
    async fn empty_cursor_is_opaque_not_terminal() {
        let mut count = 0;
        let result = collect(
            |cursor| {
                count += 1;
                ready(Ok(if count == 1 {
                    assert_eq!(cursor, None);
                    json!({"tools":[],"nextCursor":""})
                } else {
                    assert_eq!(cursor.as_deref(), Some(""));
                    json!({"tools":[tool("next")]})
                }))
            },
            DiscoveryLimits::default(),
        )
        .await
        .unwrap();
        assert_eq!(result, json!({"tools":[tool("next")]}));
        assert_eq!(count, 2);
    }

    #[tokio::test]
    async fn empty_catalog_is_distinct_from_malformed_response() {
        assert_eq!(
            pages(vec![json!({"tools":[]})], DiscoveryLimits::default())
                .await
                .unwrap(),
            json!({"tools":[]})
        );
        for page in [json!({}), json!({"tools":null}), json!([])] {
            assert!(pages(vec![page], DiscoveryLimits::default()).await.is_err());
        }
    }

    #[tokio::test]
    async fn rejects_duplicates_and_repeated_cursors() {
        for (input, expected) in [
            (
                vec![json!({"tools":[tool("x"),tool("x")]})],
                "duplicate tool",
            ),
            (
                vec![
                    json!({"tools":[tool("x")],"nextCursor":"n"}),
                    json!({"tools":[tool("x")]}),
                ],
                "duplicate tool",
            ),
            (
                vec![
                    json!({"tools":[],"nextCursor":"n"}),
                    json!({"tools":[],"nextCursor":"n"}),
                ],
                "repeated pagination",
            ),
        ] {
            assert!(
                pages(input, DiscoveryLimits::default())
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[tokio::test]
    async fn rejects_invalid_cursor_or_name() {
        for page in [
            json!({"tools":[],"nextCursor":null}),
            json!({"tools":[],"nextCursor":3}),
            json!({"tools":[{}]}),
            json!({"tools":[{"name":""}]}),
        ] {
            assert!(pages(vec![page], DiscoveryLimits::default()).await.is_err());
        }
    }

    #[tokio::test]
    async fn page_limit_never_returns_a_partial_catalog() {
        let limits = DiscoveryLimits {
            max_pages: 1,
            ..DiscoveryLimits::default()
        };
        assert!(
            pages(vec![json!({"tools":[tool("x")]})], limits)
                .await
                .is_ok()
        );
        assert!(
            pages(
                vec![json!({"tools":[tool("x")],"nextCursor":"next"})],
                limits
            )
            .await
            .unwrap_err()
            .to_string()
            .contains("page limit")
        );
    }

    #[tokio::test]
    async fn tool_and_byte_limits_are_exact() {
        let page = json!({"tools":[tool("x"),tool("y")]});
        let size = page.to_string().len();
        let limits = DiscoveryLimits {
            max_tools: 2,
            max_catalog_bytes: size,
            ..DiscoveryLimits::default()
        };
        assert_eq!(
            pages(vec![page.clone()], limits)
                .await
                .unwrap()
                .to_string()
                .len(),
            size
        );
        for (limits, expected) in [
            (
                DiscoveryLimits {
                    max_tools: 1,
                    ..limits
                },
                "tool limit",
            ),
            (
                DiscoveryLimits {
                    max_catalog_bytes: size - 1,
                    ..limits
                },
                "byte limit",
            ),
        ] {
            assert!(
                pages(vec![page.clone()], limits)
                    .await
                    .unwrap_err()
                    .to_string()
                    .contains(expected)
            );
        }
    }

    #[tokio::test]
    async fn validates_limits_before_requests() {
        let mut requested = false;
        let result = collect(
            |_| {
                requested = true;
                ready(Err(Error::Transport("unexpected request".into())))
            },
            DiscoveryLimits {
                max_pages: 0,
                ..DiscoveryLimits::default()
            },
        )
        .await;
        assert!(result.is_err());
        assert!(!requested);
    }
}
