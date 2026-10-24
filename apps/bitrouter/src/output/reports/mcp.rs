//! Reports for `mcp check` and legacy `mcp search` registry discovery,
//! distinct from the runtime upstream MCP gateway
//! verbs, which emit no report envelope).

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Human, Table};

#[derive(Serialize)]
pub struct McpCheckRow {
    pub server: String,
    pub transport: String,
    pub ok: bool,
    pub latency_ms: u128,
    pub capabilities: Vec<String>,
    pub tools: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct McpCheckReport {
    pub servers: Vec<McpCheckRow>,
}

impl CliReport for McpCheckReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let mut table = Table::new(["SERVER", "STATUS", "TRANSPORT", "LATENCY", "TOOLS"]);
        for server in &self.servers {
            table.push([
                server.server.clone(),
                if server.ok {
                    "ok".into()
                } else {
                    "FAIL".into()
                },
                server.transport.clone(),
                format!("{}ms", server.latency_ms),
                if server.ok {
                    server.tools.join(", ")
                } else {
                    server.error.clone().unwrap_or_default()
                },
            ]);
        }
        h.table(&table)
    }
}

/// One server in the MCP registry (`mcp search`).
#[derive(Serialize)]
pub struct McpRegistryRow {
    /// Reverse-DNS registry name (the `mcp add` argument).
    pub name: String,
    pub version: String,
    /// How the entry installs: `remote` (zero-install), `npx` / `uvx`
    /// (stub-able), `manual` (other package types), or `-` (no distribution).
    pub install: String,
    pub description: String,
}

/// Result of legacy `bro mcp search <query>`.
#[derive(Serialize)]
pub struct McpRegistryReport {
    pub servers: Vec<McpRegistryRow>,
    /// True when the rows came from the on-disk cache rather than a live
    /// fetch (fresh cache hit, or stale fallback after a network failure).
    pub from_cache: bool,
}

impl CliReport for McpRegistryReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let mut t = Table::new(["NAME", "VERSION", "INSTALL", "DESCRIPTION"]);
        for s in &self.servers {
            t.push([
                s.name.clone(),
                s.version.clone(),
                s.install.clone(),
                s.description.clone(),
            ]);
        }
        h.table(&t)?;
        h.line("")?;
        h.line(&format!(
            "  save a server with: {} mcp add <local-name> --url <url> (or --command <program>)",
            bitrouter_sdk::invocation::name()
        ))?;
        Ok(())
    }
}
