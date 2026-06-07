//! BitRouter origin MCP server — exposes BitRouter's own tools
//! (`complete` / `list_models` / `status`) over stdio and streamable HTTP.
//!
//! Distinct from the MCP *gateway* in `bitrouter-sdk::mcp`, which proxies
//! *upstream* MCP servers. This crate is the *origin* server for BitRouter's
//! own capabilities.

pub mod backend;
pub mod install;
pub mod server;

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
}

/// Run the MCP server to completion.
pub async fn serve(opts: ServeOptions) -> anyhow::Result<()> {
    let backend = server::build_backend(
        opts.backend,
        &opts.local_url,
        &opts.cloud_url,
        opts.cloud_token.as_deref(),
    )?;
    match opts.transport {
        Transport::Stdio => server::serve_stdio(backend).await,
        Transport::Http => server::serve_http(backend, &opts.bind).await,
    }
}
