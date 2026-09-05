//! Reports for the `mcp search` / `mcp list` / `mcp add` commands (MCP
//! registry discovery — distinct from the origin-server `mcp serve/install`
//! verbs, which emit no report envelope).

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Human, Table};

/// One server in the MCP registry (`mcp list` / `mcp search`).
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

/// Result of `bro mcp list` / `bro mcp search <query>`.
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
            "  add a server with: {} mcp add <name>",
            bitrouter_sdk::invocation::name()
        ))?;
        Ok(())
    }
}

/// Result of `bro mcp add <name>` — the paste-able YAML stub plus the
/// derived `mcp_servers:` key.
#[derive(Serialize)]
pub struct McpAddReport {
    /// Registry name the stub was generated from.
    pub name: String,
    /// The derived `mcp_servers:` key.
    pub id: String,
    pub yaml: String,
}

impl CliReport for McpAddReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        for line in self.yaml.lines() {
            h.line(line)?;
        }
        Ok(())
    }
}
