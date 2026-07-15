//! Test-only stdio entrypoint: serves the BitRouter origin MCP server over
//! stdio against a `LocalBackend`. Exists so the `stdio_smoke` integration
//! test can spawn a real process and drive the MCP handshake. Not a product
//! binary — the shipping CLI lives in `apps/bitrouter`.

use bitrouter_mcp::server::BitrouterMcp;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let server = BitrouterMcp::builder()
        .completion_local("http://127.0.0.1:4356")
        .build();
    bitrouter_mcp::server::serve_stdio(server, None).await
}
