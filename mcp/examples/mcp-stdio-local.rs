//! Test-only stdio entrypoint: serves the BitRouter origin MCP server over
//! stdio against a `LocalBackend`. Exists so the `stdio_smoke` integration
//! test can spawn a real process and drive the MCP handshake. Not a product
//! binary — the shipping CLI lives in `apps/bitrouter`.

use std::sync::Arc;

use bitrouter_mcp::backend::local::LocalBackend;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let backend = Arc::new(LocalBackend::new("http://127.0.0.1:4356"));
    bitrouter_mcp::server::serve_stdio(backend).await
}
