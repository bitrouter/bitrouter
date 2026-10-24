//! Optional setup configures MCP; skills acquisition belongs to npx skills.
use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::Value;
use std::path::{Path, PathBuf};
#[derive(Debug, Clone, Default, clap::Args)]
pub struct Options {
    /// Add one MCP server from a complete entry file.
    #[arg(long, requires = "mcp_from", value_name = "NAME")]
    pub mcp_name: Option<String>,
    /// MCP entry YAML/JSON; '-' reads stdin in headless mode.
    #[arg(long, requires = "mcp_name", value_name = "FILE|-")]
    pub mcp_from: Option<PathBuf>,
}
impl Options {
    pub(super) fn validate(&self, interactive: bool) -> Result<()> {
        ensure!(
            self.mcp_name.is_some() == self.mcp_from.is_some(),
            "--mcp-name and --mcp-from must be paired"
        );
        ensure!(
            !interactive || self.mcp_from.as_deref() != Some(Path::new("-")),
            "the wizard owns stdin; use an MCP entry file or init --yes --mcp-from -"
        );
        Ok(())
    }
}
#[derive(Debug, Clone, Serialize)]
pub struct Outcome {
    pub kind: &'static str,
    pub status: &'static str,
    pub target: Option<String>,
    pub receipt: Option<Value>,
    pub failure: Option<String>,
    pub reason: Option<&'static str>,
}

impl Outcome {
    fn skipped(kind: &'static str, reason: &'static str) -> Self {
        Self {
            kind,
            status: "skipped",
            target: None,
            receipt: None,
            failure: None,
            reason: Some(reason),
        }
    }
}

/// The CLI emits this partial report exactly once with a failing exit code.
#[derive(Debug)]
pub struct Incomplete {
    pub report: super::OnboardingReport,
}

impl std::fmt::Display for Incomplete {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Router configuration is saved; optional resource setup failed. Inspect saved state, then retry with init or the MCP management commands.")
    }
}
impl std::error::Error for Incomplete {}

async fn attempt(
    kind: &'static str,
    target: Option<String>,
    status: &'static str,
    operation: impl std::future::Future<Output = Result<Value>>,
) -> Result<Outcome> {
    match operation.await {
        Ok(receipt) => Ok(Outcome {
            kind,
            status,
            target,
            receipt: Some(receipt),
            failure: None,
            reason: None,
        }),
        Err(error) if crate::prompt::cancelled(&error) => Err(error),
        Err(error) => {
            let failure: String = error.to_string().chars().take(4096).collect();
            eprintln!(
                "Optional {kind} step failed: {failure}. Router configuration is saved; inspect saved state before retrying."
            );
            Ok(Outcome {
                kind,
                status: "failed",
                target,
                receipt: None,
                failure: Some(failure),
                reason: None,
            })
        }
    }
}

async fn mcp(config: &Path, name: &str, from: Option<PathBuf>) -> Result<Outcome> {
    attempt(
        "mcp",
        Some(name.into()),
        "saved",
        crate::mcp_config::add_for_setup(config, name, from),
    )
    .await
}

pub(super) async fn run(
    options: &Options,
    config: &Path,
    interactive: bool,
) -> Result<Vec<Outcome>> {
    options.validate(interactive)?;
    let mut outcomes = Vec::new();
    if let Some(name) = &options.mcp_name {
        outcomes.push(mcp(config, name, options.mcp_from.clone()).await?);
    }
    if interactive {
        loop {
            let action=crate::prompt::select("Optional resources","Router configuration is saved. Saving MCP does not connect or grant skill-source permission. Install skills with npx skills; inspect and edit existing directories through bro skills.",vec![crate::prompt::Item::new("Continue","finish setup"),crate::prompt::Item::new("Add MCP server","configure an upstream server")],0).await?;
            if action == 0 {
                break;
            }
            let name = crate::prompt::line("MCP server name").await?;
            outcomes.push(mcp(config, &name, None).await?);
        }
    }
    if outcomes.is_empty() {
        outcomes.push(Outcome::skipped("mcp", "not_requested"));
    }
    Ok(outcomes)
}
