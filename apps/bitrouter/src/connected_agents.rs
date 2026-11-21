//! Read-only owner-local connection projection. Runtime owners retain authority.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};

use crate::daemon::{self, DaemonCommand, DaemonResponse};
use crate::output::CliReport;
use crate::output::human::{Human, Table};

/// Match the existing native Thread directory's maximum page size.
pub const MAX_PAGE: usize = 16;

#[derive(Debug, Clone, clap::Args)]
pub struct Options {
    /// Configuration source used only to locate the daemon.
    #[arg(short, long)]
    pub config: Option<PathBuf>,
    /// Explicit owner-local control socket. Never starts the daemon.
    #[arg(long)]
    pub socket: Option<PathBuf>,
    /// Continue after the previous snapshot's next_cursor (same daemon boot).
    #[arg(long)]
    pub after: Option<String>,
    /// Maximum instances per page (1–16).
    #[arg(long, default_value = "16", value_parser = clap::value_parser!(u16).range(1..=16))]
    pub limit: u16,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Kind {
    NativeWorker,
    SupervisedAcp,
    NativeAcpAttachment,
    ManagedCoreChannel,
    ManagedNativeSession,
}

impl Kind {
    fn name(self) -> &'static str {
        match self {
            Self::NativeWorker => "native_worker",
            Self::SupervisedAcp => "supervised_acp",
            Self::NativeAcpAttachment => "native_acp_attachment",
            Self::ManagedCoreChannel => "managed_core_channel",
            Self::ManagedNativeSession => "managed_native_session",
        }
    }
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Instance {
    pub kind: Kind,
    pub id: String,
    pub agent: String,
    pub state: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub run_id: Option<String>,
    pub session_id: Option<String>,
    pub agent_session_id: Option<String>,
    pub via: Option<String>,
    pub connection_id: Option<String>,
    pub parent_run_id: Option<String>,
    pub harness_id: Option<String>,
    pub execution_epoch: Option<u64>,
    pub runtime_instance_id: Option<String>,
    pub model: Option<String>,
    pub model_selector: Option<String>,
    pub model_source: Option<String>,
    pub resource_binding: Option<String>,
}

impl Instance {}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Coverage {
    pub source: String,
    pub status: String,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub daemon_state: String,
    pub server_instance_id: Option<String>,
    pub sampled_at: Option<chrono::DateTime<chrono::Utc>>,
    pub resolved_via: String,
    pub coverage: Vec<Coverage>,
    /// None means no live sample was possible, never a fabricated empty set.
    pub instances: Option<Vec<Instance>>,
    pub next_cursor: Option<String>,
    pub limit: usize,
}

pub(crate) async fn resolve_socket(
    config: Option<&Path>,
    socket: Option<&Path>,
) -> Result<PathBuf> {
    Ok(match socket {
        Some(socket) => socket.to_path_buf(),
        None => {
            if let Some(located) = crate::daemon_locator::locate(config).await? {
                located.socket().to_path_buf()
            } else {
                let source = crate::daemon_locator::selected_source(config)?;
                match &source {
                    crate::paths::ConfigSource::Default { home } => home.join("bitrouter.sock"),
                    crate::paths::ConfigSource::File(path) => {
                        // Passive inspection still works before onboarding or
                        // after a selected YAML is removed. A verified locator
                        // already took precedence; only an absent file uses the
                        // conventional endpoint. Malformed existing input errs.
                        let control_socket = if path.try_exists()? {
                            crate::paths::load_config(&source)
                                .await?
                                .server
                                .control_socket
                        } else {
                            daemon::DEFAULT_CONTROL_SOCKET.into()
                        };
                        daemon::resolve_socket_path(path, &control_socket)
                    }
                }
            }
        }
    })
}

pub async fn query(options: &Options) -> Result<Report> {
    let socket = resolve_socket(options.config.as_deref(), options.socket.as_deref()).await?;
    let response = tokio::time::timeout(
        Duration::from_secs(5),
        daemon::send_command(
            &socket,
            &DaemonCommand::ConnectedAgents {
                after: options.after.clone(),
                limit: usize::from(options.limit),
            },
        ),
    )
    .await
    .map_err(|_| {
        anyhow::anyhow!("connected agents query timed out; connection state is unknown")
    })?;
    match response {
        Ok(DaemonResponse::ConnectedAgents { report }) => Ok(*report),
        Ok(DaemonResponse::Error { message }) => bail!("connected agents unavailable: {message}"),
        Ok(_) => bail!("daemon does not support the connected agents snapshot"),
        Err(error) if daemon::is_not_reachable(&error) => Ok(Report {
            daemon_state: "stopped".into(),
            server_instance_id: None,
            sampled_at: None,
            resolved_via: "owner_local_control".into(),
            coverage: vec![Coverage {
                source: "daemon".into(),
                status: "unavailable".into(),
                detail: "Selected daemon endpoint is not reachable; no connection sample".into(),
            }],
            instances: None,
            next_cursor: None,
            limit: usize::from(options.limit),
        }),
        Err(error) => Err(error),
    }
}

impl CliReport for Report {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line(&format!(
            "daemon: {} · source: {}",
            self.daemon_state, self.resolved_via
        ))?;
        if let (Some(instance), Some(sampled_at)) = (&self.server_instance_id, &self.sampled_at) {
            human.line(&format!("instance: {instance} · sampled_at: {sampled_at}"))?;
        }
        if let Some(rows) = &self.instances {
            let mut table = Table::new([
                "KIND",
                "INSTANCE",
                "AGENT",
                "STATE",
                "BINDING",
                "MODEL",
                "RESOURCES",
            ]);
            for row in rows {
                let mut binding = row
                    .thread_id
                    .as_ref()
                    .or(row.run_id.as_ref())
                    .or(row.session_id.as_ref())
                    .cloned()
                    .unwrap_or_else(|| "unknown".into());
                if let Some(parent) = &row.parent_run_id {
                    binding.push_str(&format!(" parent:{parent}"));
                }
                if let Some(harness) = &row.harness_id {
                    binding.push_str(&format!(" harness:{harness}"));
                }
                table.push([
                    row.kind.name().into(),
                    row.id.clone(),
                    row.agent.clone(),
                    row.state.clone(),
                    binding,
                    row.model.clone().unwrap_or_else(|| "unknown".into()),
                    row.resource_binding
                        .clone()
                        .unwrap_or_else(|| "unknown".into()),
                ]);
            }
            human.table(&table)?;
            if rows.is_empty() {
                human.line("No connected agents in the observed scope.")?;
            }
        } else {
            human.line("Connected agents: unavailable (no live sample).")?;
        }
        for coverage in &self.coverage {
            human.line(&format!(
                "{}: {} — {}",
                coverage.source, coverage.status, coverage.detail
            ))?;
        }
        if let Some(cursor) = &self.next_cursor {
            human.line(&format!("next_cursor: {cursor}"))?;
        }
        Ok(())
    }
}
