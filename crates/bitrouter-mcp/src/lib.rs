//! BitRouter's action contract, plus its MCP binding — exposing BitRouter's
//! own tools (`complete` / `list_models` / `status`) over stdio and streamable
//! HTTP.
//!
//! Distinct from the MCP *gateway* in `bitrouter-sdk::mcp`, which proxies
//! *upstream* MCP servers. This crate is the *origin* server for BitRouter's
//! own capabilities.
//!
//! [`actions`] holds the shared report types and their port traits: one typed
//! answer per question, so the CLI leaf and the MCP tool cannot drift apart.
//! The implementations live app-side; this crate keeps no business logic for
//! an action it owns.

pub mod actions;
pub mod backend;
pub mod capabilities;
pub mod error;
pub mod install;
pub mod server;

use std::path::PathBuf;

/// Parameters for `install`.
pub struct InstallOptions {
    pub client: install::Client,
    /// When set, write+merge into this config path; otherwise print to stdout.
    pub config_path: Option<PathBuf>,
}

/// Render (and optionally merge+write) the MCP client config block.
pub fn install(opts: InstallOptions) -> anyhow::Result<()> {
    let block = install::render_block(opts.client);
    match opts.config_path {
        None => {
            println!("{}", serde_json::to_string_pretty(&block)?);
            Ok(())
        }
        Some(path) => {
            let mut doc: serde_json::Value = if path.exists() {
                serde_json::from_str(&std::fs::read_to_string(&path)?)
                    .map_err(|e| anyhow::anyhow!("{} is not valid JSON: {e}", path.display()))?
            } else {
                serde_json::json!({})
            };
            install::merge_into(&mut doc, &block);
            std::fs::write(&path, serde_json::to_string_pretty(&doc)?)?;
            println!("wrote bitrouter MCP server into {}", path.display());
            Ok(())
        }
    }
}

/// Which wire transport the server speaks.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Transport {
    /// Newline-delimited JSON-RPC over stdin/stdout (local clients launch this).
    Stdio,
    /// Streamable HTTP, mounted at `/mcp-control`.
    Http,
}

/// Which backend the tools route to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BackendKind {
    /// The local BYOK daemon at `127.0.0.1:4356`.
    Local,
    /// BitRouter Cloud at `api.bitrouter.ai`.
    Cloud,
}

/// Parameters for `serve`.
pub struct ServeOptions {
    pub transport: Transport,
    pub backend: BackendKind,
    /// Local daemon root. Default `http://127.0.0.1:4356`.
    pub local_url: String,
    /// Cloud root. Default `https://api.bitrouter.ai`.
    pub cloud_url: String,
    /// Bearer for the cloud backend (from `--token` / `BITROUTER_TOKEN`).
    pub cloud_token: Option<String>,
    /// HTTP bind address (only for `Transport::Http`). Default `127.0.0.1:4357`.
    pub bind: String,
    /// Optional spend annotator for tool results (stdio transport only —
    /// the HTTP transport is multi-tenant and per-caller spend isn't
    /// what the local metering database holds).
    pub cost_footer: Option<std::sync::Arc<dyn server::CostFooter>>,
    /// Optional `route` port backing `route_preview` (stdio transport only —
    /// it reads the serving machine's own routing table, which is not what a
    /// multi-tenant HTTP caller is asking about).
    pub routing: Option<std::sync::Arc<dyn actions::route::RouteQuery>>,
    /// Optional `status` port. When unset, the backend's own
    /// [`status_port`](backend::Backend::status_port) is used — which is how
    /// the cloud profile reports its remaining credit. The local profile has no
    /// such fallback: only the embedding binary can read the control socket and
    /// the metering database.
    pub status: Option<std::sync::Arc<dyn actions::status::StatusQuery>>,
    /// Optional `list_models` port. When unset, the backend's own
    /// [`models_port`](backend::Backend::models_port) is used — a
    /// `GET /v1/models` against the daemon or the cloud account. The local
    /// profile injects a better one: it reads the daemon's live routing table
    /// over the control socket and falls back to static config, so
    /// `list_models` answers with **no daemon running**.
    pub models: Option<std::sync::Arc<dyn actions::models::ModelsQuery>>,
}

/// Run the MCP server to completion: the router profile (completion, plus
/// `route_preview` when a routing port is wired). Other profiles — the skills
/// origin server — are assembled directly through
/// [`server::BitrouterMcp::builder`] by the embedding binary.
pub async fn serve(opts: ServeOptions) -> anyhow::Result<()> {
    let backend = server::build_backend(
        opts.backend,
        opts.transport,
        &opts.local_url,
        &opts.cloud_url,
        opts.cloud_token.as_deref(),
    )?;
    match opts.transport {
        Transport::Stdio => {
            let mut builder = server::BitrouterMcp::builder().completion(backend.clone());
            // The injected port wins: on the local profile it is the control
            // socket, which knows the pid and the socket path the backend's
            // `/v1/*` client never could.
            if let Some(models) = opts.models.or_else(|| backend.clone().models_port()) {
                builder = builder.models(models);
            }
            if let Some(status) = opts.status.or_else(|| backend.status_port()) {
                builder = builder.status(status);
            }
            if let Some(routing) = opts.routing {
                builder = builder.routing(routing);
            }
            server::serve_stdio(builder.build(), opts.cost_footer).await
        }
        Transport::Http => {
            let require_auth = matches!(opts.backend, BackendKind::Cloud);
            // Without the auth middleware (local backend), a non-loopback bind
            // would expose the BYOK daemon's provider keys to the network.
            if !require_auth {
                server::ensure_loopback_bind(&opts.bind)?;
            }
            server::serve_http(backend, &opts.bind, require_auth).await
        }
    }
}
