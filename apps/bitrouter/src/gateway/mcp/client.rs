//! Bounded MCP discovery and connection ownership for application consumers.
//! Each owner retains its own transports and credentials until joined shutdown.
//! Catalog data carries no execution authority or workspace provenance.
//!
//! MCP tools, pagination and list-change notifications:
//! <https://modelcontextprotocol.io/specification/2025-11-25/server/tools>.
//! Lifecycle negotiation uses the official rmcp client:
//! <https://github.com/modelcontextprotocol/rust-sdk>.

mod http_owner;

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use super::process;
use crate::config::mcp::{McpServerConfig, McpTransport};
use rmcp::model::{
    ClientInfo, ClientRequest, Implementation, ListToolsRequest, PaginatedRequestParams,
    ProtocolVersion, ServerResult,
};
use rmcp::service::{
    ClientCacheConfig, ClientLifecycleMode, ClientServiceExt, NotificationContext,
    PeerRequestOptions, RoleClient, RunningService,
};
use rmcp::{ClientHandler, ServiceExt};
use serde::Serialize;
use serde_json::Value;
use tokio_util::sync::CancellationToken;

const MAX_SERVERS: usize = 32;
const MAX_TOOLS: usize = 256;
const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

// The original catalog included 44 bytes of local execution annotations per
// tool. Reserve the same space without assigning execution policy here, so the
// accepted catalog boundary remains byte-for-byte compatible.
const TOOL_ANNOTATION_BYTES: usize = 44;

/// A discovered tool declaration, without permission or effect classification.
#[derive(Clone, Serialize)]
pub struct CatalogTool {
    pub name: String,
    pub description: String,
    pub parameters: Value,
}

impl CatalogTool {
    pub(crate) fn catalog_bytes(&self) -> Result<usize, serde_json::Error> {
        Ok(serde_json::to_vec(self)?
            .len()
            .saturating_add(TOOL_ANNOTATION_BYTES))
    }
}

/// Discovery failure, including whether all owned processes were confirmed closed.
#[derive(Debug, thiserror::Error)]
#[error("{message}")]
pub struct ConnectError {
    pub message: String,
    pub cleanup_unknown: bool,
}

impl From<String> for ConnectError {
    fn from(message: String) -> Self {
        Self {
            message,
            cleanup_unknown: false,
        }
    }
}

#[derive(Clone)]
struct Client {
    version: ProtocolVersion,
    generation: Arc<AtomicU64>,
}

impl ClientHandler for Client {
    fn get_info(&self) -> ClientInfo {
        let mut info = ClientInfo::default();
        info.client_info = Implementation::new("bitrouter-harness", env!("CARGO_PKG_VERSION"));
        info.protocol_version = self.version.clone();
        info
    }

    async fn on_tool_list_changed(&self, _context: NotificationContext<RoleClient>) {
        self.generation.fetch_add(1, Ordering::SeqCst);
    }
}

struct Connection {
    service: RunningService<RoleClient, Client>,
    generation: Arc<AtomicU64>,
    catalog_generation: u64,
    execution_catalog: bool,
}

/// A bounded client principal owned by one application consumer, never a pool.
/// Discovery and failed initialization share the same confirmed cleanup path.
#[derive(Default)]
pub struct BoundedMcpClient {
    connections: BTreeMap<String, Connection>,
    closing: tokio::sync::Mutex<Vec<futures::future::BoxFuture<'static, Result<(), String>>>>,
    close_error: Option<String>,
    tool_names: BTreeSet<String>,
    catalog_bytes: usize,
    cleanup: Vec<process::ProcessOwner>,
    http: Vec<http_owner::Owner>,
    tools: Vec<CatalogTool>,
}

impl BoundedMcpClient {
    pub fn tools(&self) -> &[CatalogTool] {
        &self.tools
    }

    pub fn validate_catalog(&self) -> Result<(), String> {
        if self.connections.values().any(|connection| {
            connection.execution_catalog
                && (connection.catalog_generation != connection.generation.load(Ordering::SeqCst)
                    || connection.service.peer().is_transport_closed())
        }) {
            return Err(reject("MCP catalog is no longer current"));
        }
        Ok(())
    }

    pub async fn connect(
        workspace: &Path,
        servers: &[McpServerConfig],
        version: ProtocolVersion,
    ) -> Result<Self, ConnectError> {
        Self::connect_cancellable(
            workspace,
            servers,
            version,
            &CancellationToken::new(),
            Duration::from_secs(1920),
        )
        .await
    }

    pub async fn connect_cancellable(
        workspace: &Path,
        servers: &[McpServerConfig],
        version: ProtocolVersion,
        cancel: &CancellationToken,
        max_duration: Duration,
    ) -> Result<Self, ConnectError> {
        let mut clients = Self::default();
        clients
            .connect_in_place(workspace, servers, version, cancel, max_duration)
            .await?;
        Ok(clients)
    }

    // Retain cleanup owners until failed discovery has joined every transport.
    async fn connect_in_place(
        &mut self,
        workspace: &Path,
        servers: &[McpServerConfig],
        version: ProtocolVersion,
        cancel: &CancellationToken,
        max_duration: Duration,
    ) -> Result<(), ConnectError> {
        if !self.connections.is_empty() || !self.cleanup.is_empty() {
            return Err(reject("MCP discovery owner was already used").into());
        }
        if servers.len() > MAX_SERVERS {
            return Err(reject("MCP catalog exceeds 32 servers").into());
        }
        let mut names = BTreeSet::new();
        for server in servers {
            server.validate().map_err(|e| reject(e.to_string()))?;
            validate_id(&server.name)?;
            if !names.insert(&server.name) {
                return Err(reject("duplicate MCP server name").into());
            }
        }
        let workspace = workspace
            .canonicalize()
            .map_err(|_| reject("MCP workspace is unavailable"))?;
        if !workspace.is_dir() {
            return Err(reject("MCP workspace must be a directory").into());
        }
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err("MCP discovery cancelled".into()),
            _ = tokio::time::sleep(max_duration) => Err("MCP discovery exceeded the remaining Turn time".into()),
            result = self.connect_all(&workspace, servers, version, cancel) => result,
        };
        if let Err(error) = result {
            return Err(match self.shutdown().await {
                Ok(()) => ConnectError {
                    message: error,
                    cleanup_unknown: false,
                },
                Err(cleanup) => ConnectError {
                    message: format!("{error}; {cleanup}"),
                    cleanup_unknown: true,
                },
            });
        }
        Ok(())
    }

    async fn connect_all(
        &mut self,
        workspace: &Path,
        servers: &[McpServerConfig],
        version: ProtocolVersion,
        cancel: &CancellationToken,
    ) -> Result<(), String> {
        for server in servers {
            let generation = Arc::new(AtomicU64::new(0));
            let handler = Client {
                version: version.clone(),
                generation: generation.clone(),
            };
            let probe = process::ProcessOwner::new();
            self.cleanup.push(probe.clone());
            let connected = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err("MCP connection cancelled".into()),
                result = tokio::time::timeout(CONNECT_TIMEOUT, self.connect_transport(workspace, &server.transport, handler, probe)) =>
                    result.map_err(|_| reject(format!("MCP {} connection timed out", server.name))).and_then(|result| result),
            };
            let service = connected?;
            // Retain ownership before any further await, including cache setup,
            // so cancellation can explicitly close both HTTP and stdio services.
            self.connections.insert(
                server.name.clone(),
                Connection {
                    service,
                    generation,
                    catalog_generation: 0,
                    execution_catalog: false,
                },
            );
            let connection = self
                .connections
                .get_mut(&server.name)
                .ok_or_else(|| reject("MCP connection missing"))?;
            connection
                .service
                .peer()
                .set_response_cache_config(ClientCacheConfig::disabled())
                .await;
            connection.execution_catalog = connection.service.peer_info().is_some_and(|info| {
                info.instructions.is_some() || info.capabilities.tools.is_some()
            });
            if let Some(info) = connection.service.peer_info()
                && let Some(text) = &info.instructions
                && text.len() > MAX_INSTRUCTIONS_BYTES
            {
                return Err(reject("MCP server instructions exceed 64 KiB"));
            }
            if connection
                .service
                .peer_info()
                .is_some_and(|info| info.capabilities.tools.is_some())
            {
                tokio::time::timeout(CONNECT_TIMEOUT, self.discover(server))
                    .await
                    .map_err(|_| reject("MCP tool discovery exceeded its total deadline"))??;
            }
        }
        self.tools.sort_by(|a, b| a.name.cmp(&b.name));
        self.validate_catalog()
    }

    async fn discover(&mut self, server: &McpServerConfig) -> Result<(), String> {
        let connection = self
            .connections
            .get_mut(&server.name)
            .ok_or_else(|| reject("MCP connection missing"))?;
        let generation = connection.generation.load(Ordering::SeqCst);
        let prefix = server
            .tool_prefix
            .clone()
            .unwrap_or_else(|| format!("{}__", server.name));
        let mut cursor = None;
        let mut seen_cursors = BTreeSet::new();
        for _ in 0..MAX_TOOLS {
            let result = connection
                .service
                .peer()
                .send_request_with_option(
                    ClientRequest::ListToolsRequest(ListToolsRequest::with_param(
                        PaginatedRequestParams::default().with_cursor(cursor),
                    )),
                    PeerRequestOptions::with_timeout(CONNECT_TIMEOUT),
                )
                .await
                .map_err(|e| reject(format!("MCP {} tools/list: {e}", server.name)))?
                .await_response()
                .await
                .map_err(|e| reject(format!("MCP {} tools/list: {e}", server.name)))?;
            let ServerResult::ListToolsResult(page) = result else {
                return Err(reject("MCP tools/list returned an unexpected result"));
            };
            for tool in page.tools {
                if self.tools.len() >= MAX_TOOLS {
                    return Err(reject("MCP catalog exceeds 256 tools"));
                }
                let name = format!("{prefix}{}", tool.name);
                validate_id(&name)?;
                if self.tool_names.contains(&name) {
                    return Err(reject(format!("MCP tool name collision: {name}")));
                }
                let parameters = Value::Object((*tool.input_schema).clone());
                if parameters.get("type").and_then(Value::as_str) != Some("object") {
                    return Err(reject("MCP tool input schema must describe an object"));
                }
                let declaration = CatalogTool {
                    name: name.clone(),
                    description: tool.description.map(|s| s.into_owned()).unwrap_or_default(),
                    parameters,
                };
                self.catalog_bytes = self.catalog_bytes.saturating_add(
                    declaration
                        .catalog_bytes()
                        .map_err(|e| reject(e.to_string()))?,
                );
                if self.catalog_bytes > MAX_CATALOG_BYTES {
                    return Err(reject("MCP tool catalog exceeds 1 MiB"));
                }
                self.tool_names.insert(name);
                self.tools.push(declaration);
            }
            match page.next_cursor {
                None => {
                    if generation != connection.generation.load(Ordering::SeqCst) {
                        return Err(reject(
                            "MCP tools changed during discovery; rediscover before publication",
                        ));
                    }
                    connection.catalog_generation = generation;
                    return Ok(());
                }
                Some(next)
                    if !next.is_empty()
                        && next.len() <= 4096
                        && seen_cursors.insert(next.clone()) =>
                {
                    cursor = Some(next)
                }
                Some(_) => {
                    return Err(reject("MCP tools/list repeated or empty pagination cursor"));
                }
            }
        }
        Err(reject("MCP tools/list exceeded its page limit"))
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(5), self.close())
            .await
            .map_err(|_| "MCP connection cleanup timed out; cleanup is unconfirmed".to_string())?
    }

    async fn close(&mut self) -> Result<(), String> {
        self.tool_names.clear();
        self.catalog_bytes = 0;
        self.tools.clear();
        // rmcp close/cancel take the task JoinHandle into their future. Keeping
        // that SAME future here preserves the join proof across our deadline;
        // calling close again on a consumed RunningService could report Closed.
        // https://docs.rs/rmcp/3.3.0/src/rmcp/service.rs.html
        // The mutex preserves Sync for shared read/call consumers; close has
        // exclusive access, so no asynchronous lock is needed here.
        let closing = self.closing.get_mut();
        closing.extend(
            std::mem::take(&mut self.connections)
                .into_values()
                .map(|connection| {
                    Box::pin(async move {
                        connection
                            .service
                            .cancel()
                            .await
                            .map(|_| ())
                            .map_err(|_| "MCP service task did not join cleanly".to_owned())
                    })
                        as futures::future::BoxFuture<'static, Result<(), String>>
                }),
        );
        while let Some(close) = closing.last_mut() {
            let outcome = close.await;
            drop(closing.pop());
            if let Err(error) = outcome {
                self.close_error.get_or_insert(error);
            }
        }
        // Join established services first: their facade may already be driving
        // this same worker close. Pending handshakes have no service to join.
        let mut first_error = self.close_error.clone();
        for owner in &self.http {
            if let Err(error) = owner.join().await {
                first_error.get_or_insert(error);
            }
        }
        for owner in &self.cleanup {
            if let Err(error) = owner.stop().await {
                first_error.get_or_insert_with(|| {
                    format!("MCP process cleanup could not be confirmed: {error}")
                });
            }
        }
        first_error.map_or(Ok(()), Err)
    }

    async fn connect_transport(
        &mut self,
        workspace: &Path,
        transport: &McpTransport,
        handler: Client,
        cleanup: process::ProcessOwner,
    ) -> Result<RunningService<RoleClient, Client>, String> {
        let lifecycle =
            (handler.version >= ProtocolVersion::V_2026_07_28).then(|| ClientLifecycleMode::Auto {
                preferred_versions: vec![handler.version.clone()],
                legacy_version: Some(ProtocolVersion::LATEST),
            });
        match transport {
            McpTransport::Http { url, headers } => {
                let mut parsed = HashMap::new();
                for (key, value) in headers {
                    let name: http::HeaderName =
                        key.parse().map_err(|_| reject("invalid MCP header name"))?;
                    let value: http::HeaderValue = value
                        .parse()
                        .map_err(|_| reject("invalid MCP header value"))?;
                    parsed.insert(name, value);
                }
                let config = rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig::with_uri(url.clone()).custom_headers(parsed);
                let transport = http_owner::Owner::new(
                    rmcp::transport::StreamableHttpClientTransport::from_config(config),
                );
                // Register the actual worker owner before polling handshake. A
                // cancelled/failed handshake drops only its facade, never this owner.
                self.http.push(transport.clone());
                match lifecycle {
                    Some(mode) => handler.serve_with_lifecycle(transport, mode).await,
                    None => handler.serve(transport).await,
                }
                .map_err(|_| reject("MCP HTTP handshake failed"))
            }
            McpTransport::Stdio { command, args, env } => {
                let mut child = tokio::process::Command::new(command);
                child
                    .args(args)
                    .envs(env)
                    .current_dir(workspace)
                    .kill_on_drop(true);
                let transport = process::StdioTransport::spawn(child, cleanup)
                    .map_err(|_| reject("MCP stdio process could not start"))?;
                match lifecycle {
                    Some(mode) => handler.serve_with_lifecycle(transport, mode).await,
                    None => handler.serve(transport).await,
                }
                .map_err(|_| reject("MCP stdio handshake failed"))
            }
        }
    }
}

fn reject(message: impl Into<String>) -> String {
    message.into()
}

fn validate_id(name: &str) -> Result<(), String> {
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
    {
        return Err(
            "MCP names must contain 1-64 ASCII letters, digits, underscores or hyphens".into(),
        );
    }
    Ok(())
}
