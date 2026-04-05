//! MCP transport bridge — connects rmcp's client runtime to bitrouter's
//! notification and request-handler abstractions.
//!
//! [`BitrouterClientHandler`] implements rmcp's [`ClientHandler`] trait,
//! bridging server→client notifications (tool/resource/prompt list changes)
//! to [`Arc<Notify>`] handles, and forwarding sampling/elicitation requests
//! to [`McpClientRequestHandler`].
//!
//! [`ConnectedPeer`] provides a type-erased wrapper around rmcp's
//! `RunningService` so that [`UpstreamConnection`](crate::mcp::client::upstream::UpstreamConnection)
//! can hold a single field regardless of the underlying transport (HTTP or stdio).

use std::collections::HashMap;
use std::sync::Arc;

use tokio::sync::Notify;

use bitrouter_core::api::mcp::gateway::McpClientRequestHandler;
use bitrouter_core::api::mcp::types::McpGatewayError;

// ── Notify handles ─────────────────────────────────────────────────

/// Notification handles for signaling list changes to the upstream connection.
pub(crate) struct NotifyHandles {
    pub tool: Arc<Notify>,
    pub resource: Arc<Notify>,
    pub prompt: Arc<Notify>,
}

// ── ClientHandler bridge ───────────────────────────────────────────

/// Bridges rmcp's [`ClientHandler`](rmcp::handler::client::ClientHandler)
/// to bitrouter's notification and request-handler system.
pub(crate) struct BitrouterClientHandler {
    server_name: String,
    notify: NotifyHandles,
    handler: Option<Arc<dyn McpClientRequestHandler>>,
}

impl BitrouterClientHandler {
    pub fn new(
        server_name: impl Into<String>,
        notify: NotifyHandles,
        handler: Option<Arc<dyn McpClientRequestHandler>>,
    ) -> Self {
        Self {
            server_name: server_name.into(),
            notify,
            handler,
        }
    }
}

impl rmcp::handler::client::ClientHandler for BitrouterClientHandler {
    async fn create_message(
        &self,
        params: rmcp::model::CreateMessageRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleClient>,
    ) -> Result<rmcp::model::CreateMessageResult, rmcp::model::ErrorData> {
        let Some(ref handler) = self.handler else {
            return Err(rmcp::model::ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                "sampling not supported",
                None,
            ));
        };

        // Round-trip through JSON to convert between rmcp and core types.
        // Both are MCP-spec-compliant serde structs so this is lossless.
        let core_params: bitrouter_core::api::mcp::types::CreateMessageParams =
            serde_json::from_value(serde_json::to_value(&params).map_err(|e| {
                rmcp::model::ErrorData::internal_error(
                    format!("failed to serialize sampling params: {e}"),
                    None,
                )
            })?)
            .map_err(|e| {
                rmcp::model::ErrorData::internal_error(
                    format!("failed to deserialize sampling params: {e}"),
                    None,
                )
            })?;

        let result = handler
            .handle_sampling(&self.server_name, core_params)
            .await
            .map_err(|e| {
                rmcp::model::ErrorData::new(
                    rmcp::model::ErrorCode(e.code as i32),
                    e.message,
                    e.data,
                )
            })?;

        serde_json::from_value(serde_json::to_value(&result).map_err(|e| {
            rmcp::model::ErrorData::internal_error(
                format!("failed to serialize sampling result: {e}"),
                None,
            )
        })?)
        .map_err(|e| {
            rmcp::model::ErrorData::internal_error(
                format!("failed to deserialize sampling result: {e}"),
                None,
            )
        })
    }

    async fn create_elicitation(
        &self,
        params: rmcp::model::CreateElicitationRequestParams,
        _context: rmcp::service::RequestContext<rmcp::service::RoleClient>,
    ) -> Result<rmcp::model::CreateElicitationResult, rmcp::model::ErrorData> {
        let Some(ref handler) = self.handler else {
            return Err(rmcp::model::ErrorData::new(
                rmcp::model::ErrorCode::METHOD_NOT_FOUND,
                "elicitation not supported",
                None,
            ));
        };

        let core_params: bitrouter_core::api::mcp::types::ElicitationCreateParams =
            serde_json::from_value(serde_json::to_value(&params).map_err(|e| {
                rmcp::model::ErrorData::internal_error(
                    format!("failed to serialize elicitation params: {e}"),
                    None,
                )
            })?)
            .map_err(|e| {
                rmcp::model::ErrorData::internal_error(
                    format!("failed to deserialize elicitation params: {e}"),
                    None,
                )
            })?;

        let result = handler
            .handle_elicitation(&self.server_name, core_params)
            .await
            .map_err(|e| {
                rmcp::model::ErrorData::new(
                    rmcp::model::ErrorCode(e.code as i32),
                    e.message,
                    e.data,
                )
            })?;

        serde_json::from_value(serde_json::to_value(&result).map_err(|e| {
            rmcp::model::ErrorData::internal_error(
                format!("failed to serialize elicitation result: {e}"),
                None,
            )
        })?)
        .map_err(|e| {
            rmcp::model::ErrorData::internal_error(
                format!("failed to deserialize elicitation result: {e}"),
                None,
            )
        })
    }

    fn on_tool_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::service::RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.notify.tool.notify_one();
        std::future::ready(())
    }

    fn on_resource_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::service::RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.notify.resource.notify_one();
        std::future::ready(())
    }

    fn on_prompt_list_changed(
        &self,
        _context: rmcp::service::NotificationContext<rmcp::service::RoleClient>,
    ) -> impl Future<Output = ()> + Send + '_ {
        self.notify.prompt.notify_one();
        std::future::ready(())
    }

    fn get_info(&self) -> rmcp::model::ClientInfo {
        rmcp::model::ClientInfo::default()
    }
}

// ── ConnectedPeer ──────────────────────────────────────────────────

/// Type-erased wrapper around a running rmcp client service.
///
/// Both HTTP and stdio transports produce the same [`ConnectedPeer`],
/// allowing [`UpstreamConnection`] to hold a single field regardless
/// of transport type.
pub(crate) struct ConnectedPeer {
    inner: Box<dyn RunningServiceHandle>,
}

/// Object-safe handle to an rmcp `RunningService`.
trait RunningServiceHandle: Send + Sync {
    fn peer(&self) -> &rmcp::service::Peer<rmcp::service::RoleClient>;
}

struct Wrapper<S: rmcp::handler::client::ClientHandler>(
    rmcp::service::RunningService<rmcp::service::RoleClient, S>,
);

impl<S: rmcp::handler::client::ClientHandler> RunningServiceHandle for Wrapper<S> {
    fn peer(&self) -> &rmcp::service::Peer<rmcp::service::RoleClient> {
        self.0.peer()
    }
}

impl ConnectedPeer {
    /// Wrap a running rmcp service, erasing the concrete service type.
    pub fn from_service<S: rmcp::handler::client::ClientHandler>(
        service: rmcp::service::RunningService<rmcp::service::RoleClient, S>,
    ) -> Self {
        Self {
            inner: Box::new(Wrapper(service)),
        }
    }

    /// Access the underlying rmcp peer for making requests.
    pub fn peer(&self) -> &rmcp::service::Peer<rmcp::service::RoleClient> {
        self.inner.peer()
    }
}

// ── Transport construction helpers ─────────────────────────────────

/// Build a [`StreamableHttpClientTransport`] from a URL and headers.
pub(crate) fn build_http_transport(
    url: &str,
    headers: &HashMap<String, String>,
    name: &str,
) -> Result<rmcp::transport::StreamableHttpClientTransport<reqwest::Client>, McpGatewayError> {
    use reqwest::header::{HeaderMap, HeaderName, HeaderValue};

    let mut header_map = HeaderMap::new();
    for (k, v) in headers {
        let header_name: HeaderName = k.parse().map_err(|e| McpGatewayError::UpstreamConnect {
            name: name.to_owned(),
            reason: format!("invalid header name '{k}': {e}"),
        })?;
        let header_value: HeaderValue =
            v.parse().map_err(|e| McpGatewayError::UpstreamConnect {
                name: name.to_owned(),
                reason: format!("invalid header value for '{k}': {e}"),
            })?;
        header_map.insert(header_name, header_value);
    }

    let http_client = reqwest::Client::builder()
        .default_headers(header_map)
        .build()
        .map_err(|e| McpGatewayError::UpstreamConnect {
            name: name.to_owned(),
            reason: format!("failed to build HTTP client: {e}"),
        })?;

    let config =
        rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url);

    Ok(rmcp::transport::StreamableHttpClientTransport::with_client(
        http_client,
        config,
    ))
}
