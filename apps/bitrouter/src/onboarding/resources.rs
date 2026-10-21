//! Optional setup reuses the MCP publisher and versioned skill installer.

use std::path::{Path, PathBuf};

use anyhow::{Result, ensure};
use serde::Serialize;
use serde_json::Value;

#[derive(Debug, Clone, Default, clap::Args)]
pub struct Options {
    /// Add one MCP server from a complete entry file; requires --mcp-from.
    #[arg(long, requires = "mcp_from", value_name = "NAME")]
    pub mcp_name: Option<String>,
    /// Single McpServerConfig YAML/JSON. '-' reads stdin in headless mode.
    #[arg(long, requires = "mcp_name", value_name = "FILE|-")]
    pub mcp_from: Option<PathBuf>,
    /// Snapshot one local/Git skill source into the BitRouter library.
    #[arg(long, value_name = "SOURCE")]
    pub install_skill: Option<String>,
    /// Select one bundle in a multi-skill source; never inferred by --yes.
    #[arg(long, requires = "install_skill", value_name = "NAME_OR_PATH")]
    pub skill: Option<String>,
    /// Git ref for the optional skill installation.
    #[arg(long, requires = "install_skill", value_name = "REF")]
    pub skill_ref: Option<String>,
    /// Project scope for the skill library; default is the current Git root/directory.
    #[arg(long, conflicts_with = "global", value_name = "PATH")]
    pub workspace: Option<PathBuf>,
    /// Install skills in the current user's BitRouter library.
    #[arg(long, conflicts_with = "workspace")]
    pub global: bool,
}

impl Options {
    pub(super) fn validate(&self, interactive: bool) -> Result<()> {
        ensure!(
            self.mcp_name.is_some() == self.mcp_from.is_some(),
            "--mcp-name and --mcp-from must be paired"
        );
        ensure!(
            interactive
                || self.install_skill.is_some()
                || (!self.global
                    && self.workspace.is_none()
                    && self.skill.is_none()
                    && self.skill_ref.is_none()),
            "skill options require --install-skill outside the wizard"
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
        f.write_str("Router configuration is saved; optional resource setup failed. Inspect saved state, then retry with init or the MCP/skills management commands.")
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

async fn skill(options: &Options, source: &str, interactive: bool) -> Result<Outcome> {
    attempt(
        "skill",
        Some(if options.global { "global" } else { "project" }.into()),
        "installed",
        async {
            crate::skills::install_for_setup(
                source,
                options.skill.as_deref(),
                options.skill_ref.clone(),
                options.global,
                options.workspace.as_deref(),
                interactive,
            )
            .await
        },
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
    let prior_failure = outcomes.iter().any(|outcome| outcome.status == "failed");
    if let Some(source) = &options.install_skill {
        outcomes.push(if prior_failure {
            Outcome::skipped("skill", "previous_step_failed")
        } else {
            skill(options, source, interactive).await?
        });
    }
    if interactive {
        loop {
            let action = crate::prompt::select(
                "Optional resources",
                "Router configuration is saved. Add resources, retry a failed step, or Continue. Saving MCP does not connect or grant skill-source permission; skill installation never executes scripts.",
                vec![crate::prompt::Item::new("Continue", "finish setup; leave optional resources as reported"),
                    crate::prompt::Item::new("Add MCP server", "reuse the source-owned MCP configuration transaction"),
                    crate::prompt::Item::new("Install skill", "snapshot a local/Git bundle into the selected BitRouter scope")],
                0,
            ).await?;
            match action {
                0 => break,
                1 => {
                    let name = crate::prompt::line("MCP server name").await?;
                    outcomes.push(mcp(config, &name, None).await?);
                }
                _ => {
                    let source =
                        crate::prompt::line("Skill source (local directory or Git URL)").await?;
                    let mut selected = options.clone();
                    selected.skill = None;
                    selected.skill_ref = None;
                    outcomes.push(skill(&selected, &source, true).await?);
                }
            }
        }
    }
    for kind in ["mcp", "skill"] {
        if !outcomes.iter().any(|outcome| outcome.kind == kind) {
            outcomes.push(Outcome::skipped(kind, "not_requested"));
        }
    }
    Ok(outcomes)
}
