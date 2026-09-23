//! Reports for the `agents` commands.

use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::{Human, Table};
use crate::supervisor::{
    AttentionState, ProcessState, ReviewState, RunSnapshot, RunSummary, TurnState,
};

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

/// One agent in the ACP registry (`agents list --remote`).
#[derive(Serialize)]
pub struct AgentRegistryRow {
    pub id: String,
    pub version: String,
    /// How the entry installs: `npx` / `uvx` (stub-able), `manual`
    /// (binary-only), or `-` (no distribution).
    pub install: String,
    pub description: String,
}

/// Result of `bro agents list`. `registry` is present only with
/// `--remote` (the fetched ACP registry).
#[derive(Serialize)]
pub struct AgentsListReport {
    pub agents: Vec<AgentRow>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub registry: Option<Vec<AgentRegistryRow>>,
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
        h.table(&t)?;
        if let Some(registry) = &self.registry {
            h.line("")?;
            h.line(&format!("ACP registry ({} agents):", registry.len()))?;
            let mut t = Table::new(["ID", "VERSION", "INSTALL", "DESCRIPTION"]);
            for r in registry {
                t.push([
                    r.id.clone(),
                    r.version.clone(),
                    r.install.clone(),
                    r.description.clone(),
                ]);
            }
            h.table(&t)?;
            h.line("")?;
            h.line(&format!(
                "  install a stub with: {} agents install <id>",
                bitrouter_sdk::invocation::name()
            ))?;
        }
        Ok(())
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

/// Result of `bro agents check`.
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

/// Scriptable inventory returned by `agents sessions`.
#[derive(Serialize)]
pub struct AgentSessionsReport {
    pub runs: Vec<RunSummary>,
}

impl CliReport for AgentSessionsReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        let mut table = Table::new(["RUN", "LABEL", "AGENT", "STATE", "DIRECTORY", "ACTIVITY"]);
        for run in &self.runs {
            table.push([
                run.run_id.clone(),
                run.label.clone(),
                run.agent_id.clone(),
                summary_group(run).to_string(),
                run.cwd.display().to_string(),
                run.activity.clone(),
            ]);
        }
        h.table(&table)
    }
}

/// Acceptance report for `run --background`.
#[derive(Serialize)]
pub struct BackgroundRunReport {
    pub agent_run_id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub native_session_id: Option<String>,
    pub state: String,
}

impl BackgroundRunReport {
    pub fn from_snapshot(snapshot: &RunSnapshot) -> Self {
        Self {
            agent_run_id: snapshot.run_id.clone(),
            native_session_id: snapshot.native_session_id.clone(),
            state: run_group(snapshot).to_string(),
        }
    }
}

impl CliReport for BackgroundRunReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!("agent run {} · {}", self.agent_run_id, self.state))?;
        h.line(&format!(
            "attach: {} agents attach {}",
            bitrouter_sdk::invocation::name(),
            self.agent_run_id
        ))
    }
}

/// Result of a stop/remove lifecycle mutation.
#[derive(Serialize)]
pub struct AgentSessionMutationReport {
    pub action: &'static str,
    pub agent_run_id: String,
    pub state: String,
}

impl CliReport for AgentSessionMutationReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "{} {} · {}",
            self.action, self.agent_run_id, self.state
        ))
    }
}

fn run_group(run: &RunSnapshot) -> &'static str {
    match (run.attention, run.review, run.turn, run.process) {
        (
            AttentionState::Question | AttentionState::Permission | AttentionState::Error,
            _,
            _,
            _,
        ) => "needs_input",
        (AttentionState::Result, ReviewState::Unread, _, _) => "ready_for_review",
        (_, _, TurnState::Submitting | TurnState::Working | TurnState::Cancelling, _)
        | (_, _, _, ProcessState::Starting | ProcessState::Stopping) => "working",
        (_, _, _, ProcessState::Stopped | ProcessState::Failed | ProcessState::Interrupted) => {
            "stopped"
        }
        _ => "idle",
    }
}

fn summary_group(run: &RunSummary) -> &'static str {
    match (run.attention, run.review, run.turn, run.process) {
        (
            AttentionState::Question | AttentionState::Permission | AttentionState::Error,
            _,
            _,
            _,
        ) => "needs_input",
        (AttentionState::Result, ReviewState::Unread, _, _) => "ready_for_review",
        (_, _, TurnState::Submitting | TurnState::Working | TurnState::Cancelling, _)
        | (_, _, _, ProcessState::Starting | ProcessState::Stopping) => "working",
        (_, _, _, ProcessState::Stopped | ProcessState::Failed | ProcessState::Interrupted) => {
            "stopped"
        }
        _ => "idle",
    }
}

/// Result of `bro agents install <id>` — the paste-able YAML stub.
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

/// One tier's verdict in `agents conformance`.
#[derive(Serialize)]
pub struct AgentConformanceTier {
    pub tier: String,
    pub outcome: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason: Option<String>,
    pub duration_ms: u128,
}

/// Result of `bro agents conformance <id>`.
///
/// `registry_block` is the point of the command: the YAML a contributor pastes
/// under their runtime's agent entry. Printing it rather than writing it keeps
/// the recorded claim something a human chose to commit.
#[derive(Serialize)]
pub struct AgentConformanceReport {
    pub agent: String,
    pub suite: String,
    pub suite_version: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub agent_version: Option<String>,
    pub passed: bool,
    pub tiers: Vec<AgentConformanceTier>,
    pub registry_block: String,
}

impl CliReport for AgentConformanceReport {
    fn render(&self, h: &mut Human<'_>) -> std::io::Result<()> {
        h.line(&format!(
            "{} — {} {}",
            self.agent, self.suite, self.suite_version
        ))?;
        if let Some(version) = &self.agent_version {
            h.line(&format!("agent reported version {version}"))?;
        }
        h.line("")?;
        let mut t = Table::new(["TIER", "OUTCOME", "TIME", "DETAIL"]);
        for tier in &self.tiers {
            t.push([
                tier.tier.clone(),
                tier.outcome.clone(),
                format!("{}ms", tier.duration_ms),
                tier.reason.clone().unwrap_or_default(),
            ]);
        }
        h.table(&t)?;
        h.line("")?;
        if self.passed {
            h.line("paste this under the agent's entry in registry/runtimes/<runtime>.yaml:")?;
            h.line("")?;
            h.line(&self.registry_block)?;
        } else {
            h.line("no record is emitted for a run that did not pass — an absent tier means")?;
            h.line("\"not measured\", which is the honest state until the failure is fixed.")?;
        }
        Ok(())
    }
}
