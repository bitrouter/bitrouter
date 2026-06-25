//! Reports for the `tools` (MCP introspection) commands.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Human, Table};

/// One advertised tool.
#[derive(Serialize)]
pub struct ToolInfo {
    pub name: String,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub description: String,
}

/// One MCP server in `tools list`: either its advertised tools or the error
/// reaching it.
#[derive(Serialize)]
pub struct ServerToolsView {
    pub server: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<ToolInfo>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of `bitrouter tools list`.
#[derive(Serialize)]
pub struct ToolsListReport {
    pub servers: Vec<ServerToolsView>,
}

impl CliReport for ToolsListReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        for s in &self.servers {
            if let Some(err) = &s.error {
                h.line(&format!("{} — error: {err}", s.server))?;
            } else if let Some(tools) = &s.tools {
                if tools.is_empty() {
                    h.line(&format!("{} (no tools advertised)", s.server))?;
                } else {
                    h.line(&format!("{} ({})", s.server, tools.len()))?;
                    for t in tools {
                        if t.description.is_empty() {
                            h.line(&format!("  {}", t.name))?;
                        } else {
                            h.line(&format!("  {} — {}", t.name, t.description))?;
                        }
                    }
                }
            }
        }
        Ok(())
    }
}

/// One MCP server's health in `tools status`.
#[derive(Serialize)]
pub struct ServerStatusView {
    pub server: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u128>,
    pub transport: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of `bitrouter tools status`.
#[derive(Serialize)]
pub struct ToolsStatusReport {
    pub servers: Vec<ServerStatusView>,
}

impl CliReport for ToolsStatusReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let mut t = Table::new(["SERVER", "STATUS", "LATENCY", "TRANSPORT"]);
        for s in &self.servers {
            t.push([
                s.server.clone(),
                if s.ok { "ok".into() } else { "FAIL".into() },
                s.latency_ms
                    .map(|ms| format!("{ms}ms"))
                    .unwrap_or_else(|| "-".into()),
                s.transport.clone(),
            ]);
        }
        h.table(&t)
    }
}

/// Result of `bitrouter tools discover <server>` — the paste-able YAML stub,
/// carried verbatim under `yaml`.
#[derive(Serialize)]
pub struct ToolsDiscoverReport {
    pub server: String,
    pub yaml: String,
}

impl CliReport for ToolsDiscoverReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        for line in self.yaml.lines() {
            h.line(line)?;
        }
        Ok(())
    }
}
