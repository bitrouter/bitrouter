//! Harness-owned MCP connections. The model sees a frozen tool catalog, while
//! this module retains transports and credentials on the execution host.

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use bitrouter_sdk::mcp::transport::{McpServerConfig, McpTransport};
use rmcp::model::{
    CallToolRequest, CallToolRequestParams, ClientInfo, ClientRequest, Implementation,
    ListToolsRequest, PaginatedRequestParams, ProtocolVersion, ServerResult,
};
use rmcp::service::{
    ClientCacheConfig, ClientLifecycleMode, ClientServiceExt, NotificationContext,
    PeerRequestOptions, RoleClient, RunningService,
};
use rmcp::{ClientHandler, ServiceExt};
use serde_json::Value;
use tokio_util::sync::CancellationToken;

use super::sha256;
use super::{MaterialRef, McpTool, ResourceError, validate_id};
use crate::store::EffectStatus;

mod process;

const MAX_SERVERS: usize = 32;
const MAX_TOOLS: usize = 256;
const MAX_CATALOG_BYTES: usize = 1024 * 1024;
const MAX_INSTRUCTIONS_BYTES: usize = 64 * 1024;
const CALL_TIMEOUT: Duration = Duration::from_secs(120);
const CONNECT_TIMEOUT: Duration = Duration::from_secs(30);

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
}

struct Binding {
    server: String,
    upstream_name: String,
}

/// One MCP client principal per harness resource owner, never a gateway pool.
/// Server annotations do not grant local read-only or approval permissions.
#[derive(Default)]
pub struct McpConnections {
    connections: BTreeMap<String, Connection>,
    bindings: BTreeMap<String, Binding>,
    catalog_bytes: usize,
    cleanup: Vec<process::ProcessOwner>,
    pub(super) tools: Vec<McpTool>,
    pub(super) instructions: Vec<MaterialRef>,
}

impl McpConnections {
    pub fn tools(&self) -> &[McpTool] {
        &self.tools
    }

    pub fn validate_catalog(&self) -> Result<(), String> {
        if self.connections.values().any(|connection| {
            connection.catalog_generation != connection.generation.load(Ordering::SeqCst)
                || connection.service.peer().is_transport_closed()
        }) {
            return Err(reject("MCP catalog is no longer current"));
        }
        Ok(())
    }

    pub async fn connect(
        workspace: &Path,
        servers: &[McpServerConfig],
        version: ProtocolVersion,
    ) -> Result<Self, ResourceError> {
        Self::connect_cancellable(
            workspace,
            servers,
            version,
            &CancellationToken::new(),
            Duration::from_secs(1920),
        )
        .await
    }

    pub(crate) async fn connect_cancellable(
        workspace: &Path,
        servers: &[McpServerConfig],
        version: ProtocolVersion,
        cancel: &CancellationToken,
        max_duration: Duration,
    ) -> Result<Self, ResourceError> {
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
        let mut clients = Self::default();
        let result = tokio::select! {
            biased;
            _ = cancel.cancelled() => Err("MCP discovery cancelled".into()),
            _ = tokio::time::sleep(max_duration) => Err("MCP discovery exceeded the remaining Turn time".into()),
            result = clients.connect_all(&workspace, servers, version, cancel) => result,
        };
        if let Err(error) = result {
            return Err(match clients.shutdown().await {
                Ok(()) => ResourceError {
                    message: error,
                    cleanup_unknown: false,
                },
                Err(cleanup) => ResourceError {
                    message: format!("{error}; {cleanup}"),
                    cleanup_unknown: true,
                },
            });
        }
        Ok(clients)
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
            let service = tokio::select! {
                biased;
                _ = cancel.cancelled() => return Err("MCP connection cancelled".into()),
                result = tokio::time::timeout(CONNECT_TIMEOUT, connect(workspace, &server.transport, handler, probe)) =>
                    result.map_err(|_| reject(format!("MCP {} connection timed out", server.name)))??,
            };
            service
                .peer()
                .set_response_cache_config(ClientCacheConfig::disabled())
                .await;
            if let Some(info) = service.peer_info()
                && let Some(text) = &info.instructions
            {
                if text.len() > MAX_INSTRUCTIONS_BYTES {
                    // Retain the service so the error path explicitly closes it.
                    self.connections.insert(
                        server.name.clone(),
                        Connection {
                            service,
                            generation,
                            catalog_generation: 0,
                        },
                    );
                    return Err(reject("MCP server instructions exceed 64 KiB"));
                }
                let digest = sha256(text.as_bytes());
                self.instructions.push(MaterialRef {
                    material_id: format!("mcp_instructions_{}", sha256(server.name.as_bytes())),
                    version: digest.clone(),
                    sha256: digest,
                    media_type: "text/plain".into(),
                    provenance: format!("mcp_server:{}", server.name),
                });
            }
            self.connections.insert(
                server.name.clone(),
                Connection {
                    service,
                    generation,
                    catalog_generation: 0,
                },
            );
            tokio::time::timeout(CONNECT_TIMEOUT, self.discover(server))
                .await
                .map_err(|_| reject("MCP tool discovery exceeded its total deadline"))??;
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
                if self.bindings.contains_key(&name) {
                    return Err(reject(format!("MCP tool name collision: {name}")));
                }
                let parameters = Value::Object((*tool.input_schema).clone());
                if parameters.get("type").and_then(Value::as_str) != Some("object") {
                    return Err(reject("MCP tool input schema must describe an object"));
                }
                let declaration = McpTool {
                    name: name.clone(),
                    description: tool.description.map(|s| s.into_owned()).unwrap_or_default(),
                    parameters,
                    effect: EffectStatus::Unknown,
                    approval_required: true,
                };
                self.catalog_bytes = self.catalog_bytes.saturating_add(
                    serde_json::to_vec(&declaration)
                        .map_err(|e| reject(e.to_string()))?
                        .len(),
                );
                if self.catalog_bytes > MAX_CATALOG_BYTES {
                    return Err(reject("MCP tool catalog exceeds 1 MiB"));
                }
                self.bindings.insert(
                    name,
                    Binding {
                        server: server.name.clone(),
                        upstream_name: tool.name.into_owned(),
                    },
                );
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

    /// Called only after host permission, durable invocation/start-fence and
    /// ownership checks. A timeout or cancellation after send is an unknown
    /// effect, never an instruction to retry tools/call.
    pub async fn call(
        &self,
        name: &str,
        arguments: &Value,
        output_bytes: u64,
        cancel: &CancellationToken,
    ) -> Result<(EffectStatus, String), String> {
        let binding = self
            .bindings
            .get(name)
            .ok_or_else(|| reject("MCP tool was not advertised"))?;
        let connection = self
            .connections
            .get(&binding.server)
            .ok_or_else(|| reject("MCP connection unavailable"))?;
        if connection.catalog_generation != connection.generation.load(Ordering::SeqCst)
            || connection.service.peer().is_transport_closed()
        {
            return Err(reject(
                "MCP tool catalog/connection changed; reconnect and publish a new manifest",
            ));
        }
        if cancel.is_cancelled() {
            return Ok((
                EffectStatus::NotExecuted,
                "cancelled before MCP dispatch".into(),
            ));
        }
        let args = arguments
            .as_object()
            .cloned()
            .ok_or_else(|| reject("MCP arguments must be an object"))?;
        let params = CallToolRequestParams::new(binding.upstream_name.clone()).with_arguments(args);
        let peer = connection.service.peer();
        let deadline = tokio::time::Instant::now() + CALL_TIMEOUT;
        let send = peer.send_request_with_option(
            ClientRequest::CallToolRequest(CallToolRequest::new(params)),
            PeerRequestOptions::no_options(),
        );
        let mut handle = tokio::select! {
            biased;
            _ = cancel.cancelled() => return Ok((EffectStatus::Unknown, "MCP dispatch interrupted; reconcile the upstream effect".into())),
            _ = tokio::time::sleep_until(deadline) => return Ok((EffectStatus::Unknown, "MCP dispatch timed out; reconcile the upstream effect".into())),
            result = send => match result {
                Ok(handle) => handle,
                Err(_) => return Ok((EffectStatus::Unknown, "MCP dispatch failed; reconcile the upstream effect".into())),
            },
        };
        // Keep the handle while waiting so cancellation retires the request
        // through rmcp's own cancellation API. No progress/subscription options
        // are installed for this single synchronous tools/call.
        let response = tokio::select! {
            biased;
            _ = cancel.cancelled() => None,
            _ = tokio::time::sleep_until(deadline) => None,
            response = &mut handle.rx => Some(response),
        };
        let response = match response {
            Some(Ok(response)) => response,
            Some(Err(_)) => {
                return Ok((
                    EffectStatus::Unknown,
                    "MCP connection ended before an outcome; reconcile the upstream effect".into(),
                ));
            }
            None => {
                let _ = tokio::time::timeout(
                    Duration::from_secs(5),
                    handle.cancel(Some("harness cancellation or deadline".into())),
                )
                .await;
                return Ok((
                    EffectStatus::Unknown,
                    "MCP call cancelled or timed out; reconcile the upstream effect".into(),
                ));
            }
        };
        let result = match response {
            Ok(ServerResult::CallToolResult(result)) => result,
            _ => {
                return Ok((
                    EffectStatus::Unknown,
                    "MCP call did not return a complete outcome; reconcile the upstream effect"
                        .into(),
                ));
            }
        };
        let Ok(output) = serde_json::to_string(&result) else {
            return Ok((
                EffectStatus::Unknown,
                "MCP outcome could not be retained; reconcile the upstream effect".into(),
            ));
        };
        if output.len() as u64 > output_bytes {
            return Ok((
                EffectStatus::Unknown,
                "MCP result exceeds the frozen output allowance; reconcile the upstream effect"
                    .into(),
            ));
        }
        let status = EffectStatus::Completed;
        Ok((status, output))
    }

    pub async fn shutdown(&mut self) -> Result<(), String> {
        tokio::time::timeout(Duration::from_secs(5), self.close())
            .await
            .map_err(|_| "MCP connection cleanup timed out; cleanup is unconfirmed".to_string())?
    }

    async fn close(&mut self) -> Result<(), String> {
        self.bindings.clear();
        self.catalog_bytes = 0;
        self.tools.clear();
        self.instructions.clear();
        let mut first_error = None;
        for (_, connection) in std::mem::take(&mut self.connections) {
            if let Err(error) = connection.service.cancel().await {
                first_error.get_or_insert_with(|| reject(error.to_string()));
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
}

async fn connect(
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
            let transport = rmcp::transport::StreamableHttpClientTransport::from_config(config);
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

fn reject(message: impl Into<String>) -> String {
    message.into()
}
