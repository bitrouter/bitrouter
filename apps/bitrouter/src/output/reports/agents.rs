//! Reports for the `agents` commands.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Human, Table};

fn yesno(b: bool) -> String {
    if b { "yes".into() } else { "no".into() }
}

/// One agent in `agents list`.
#[derive(Serialize)]
pub struct AgentRow {
    pub id: String,
    pub configured: bool,
    pub in_catalog: bool,
    pub description: String,
}

/// Result of `bitrouter agents list`.
#[derive(Serialize)]
pub struct AgentsListReport {
    pub agents: Vec<AgentRow>,
}

impl CliReport for AgentsListReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let mut t = Table::new(["ID", "CONFIGURED", "CATALOG", "DESCRIPTION"]);
        for a in &self.agents {
            t.push([
                a.id.clone(),
                yesno(a.configured),
                yesno(a.in_catalog),
                a.description.clone(),
            ]);
        }
        h.table(&t)
    }
}

/// One agent's `initialize` health in `agents check`.
#[derive(Serialize)]
pub struct AgentCheckRow {
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub latency_ms: Option<u128>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

/// Result of `bitrouter agents check`.
#[derive(Serialize)]
pub struct AgentsCheckReport {
    pub agents: Vec<AgentCheckRow>,
}

impl CliReport for AgentsCheckReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let mut t = Table::new(["AGENT", "STATUS", "LATENCY"]);
        for a in &self.agents {
            t.push([
                a.id.clone(),
                if a.ok { "ok".into() } else { "FAIL".into() },
                a.latency_ms
                    .map(|ms| format!("{ms}ms"))
                    .unwrap_or_else(|| "-".into()),
            ]);
        }
        h.table(&t)
    }
}

/// Result of `bitrouter agents install <id>` — the paste-able YAML stub.
#[derive(Serialize)]
pub struct AgentInstallReport {
    pub id: String,
    pub yaml: String,
}

impl CliReport for AgentInstallReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        for line in self.yaml.lines() {
            h.line(line)?;
        }
        Ok(())
    }
}
