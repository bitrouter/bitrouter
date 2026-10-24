//! Read-only owner-local connection projection. Runtime owners retain authority.

use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{Result, bail, ensure};
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

impl Instance {
    fn key(&self) -> String {
        format!("{}:{}", self.kind.name(), self.id)
    }
}

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

/// References to the actual native owners, shared with the host listeners.
#[derive(Clone)]
pub struct NativeOwners {
    pub threads: bitrouter_orchestrator::service::ThreadService,
    pub acp: bitrouter_orchestrator::acp::native::NativeAcpServer,
    pub managed: crate::orchestrator_api::ManagedCoreApi,
    pub(crate) managed_native: crate::managed_task::owner::Owner,
}

pub async fn snapshot(
    instance: &str,
    supervisor: Option<&crate::supervisor::Supervisor>,
    native: Option<&NativeOwners>,
    after: Option<&str>,
    limit: usize,
) -> Result<Report> {
    ensure!(
        (1..=MAX_PAGE).contains(&limit),
        "limit must be 1–{MAX_PAGE}"
    );
    let after_key = after
        .map(|cursor| {
            let (boot, key) = cursor
                .split_once('/')
                .ok_or_else(|| anyhow::anyhow!("invalid connection cursor"))?;
            ensure!(
                boot == instance && !key.is_empty() && key.len() <= 1024,
                "connection cursor belongs to another daemon boot or is invalid"
            );
            Ok::<_, anyhow::Error>(key)
        })
        .transpose()?;
    let mut rows = Vec::new();
    let mut coverage = Vec::new();
    if let Some(native) = native {
        let native_instance = native.threads.capabilities().server_instance_id;
        for worker in native.threads.active_workers() {
            rows.push(Instance {
                kind: Kind::NativeWorker,
                id: worker.turn_id.clone(),
                agent: "native".into(),
                state: serde_json::to_value(worker.status)?
                    .as_str()
                    .unwrap_or("unknown")
                    .into(),
                thread_id: Some(worker.thread_id),
                turn_id: Some(worker.turn_id),
                run_id: None,
                session_id: None,
                agent_session_id: None,
                via: None,
                connection_id: None,
                parent_run_id: None,
                harness_id: None,
                execution_epoch: None,
                runtime_instance_id: Some(native_instance.clone()),
                model: None,
                model_selector: Some(worker.model),
                model_source: None,
                resource_binding: worker.resource_binding,
            });
        }
        for (connection, thread) in native.acp.connected_attachments().await {
            rows.push(Instance {
                kind: Kind::NativeAcpAttachment,
                id: format!("{connection}:{thread}"),
                agent: "native".into(),
                state: "attached".into(),
                thread_id: Some(thread.clone()),
                turn_id: None,
                run_id: None,
                session_id: Some(thread),
                agent_session_id: None,
                via: None,
                connection_id: Some(connection),
                parent_run_id: None,
                harness_id: None,
                execution_epoch: None,
                runtime_instance_id: Some(native_instance.clone()),
                model: None,
                model_selector: None,
                model_source: None,
                resource_binding: None,
            });
        }
        rows.extend(native.managed.connected_sessions().await?);
        for report in native.managed_native.active_sessions().await? {
            let Some(grant) = report.grant else {
                continue;
            };
            rows.push(Instance {
                kind: Kind::ManagedNativeSession,
                id: grant.session_id.clone(),
                agent: "native_managed".into(),
                state: report.state,
                thread_id: None,
                turn_id: None,
                run_id: report.run.as_ref().map(|run| run.run_id.clone()),
                session_id: Some(grant.session_id),
                agent_session_id: None,
                via: None,
                connection_id: None,
                parent_run_id: None,
                harness_id: Some(grant.harness_id),
                execution_epoch: Some(grant.execution_epoch),
                runtime_instance_id: Some(grant.core_instance_id),
                model: None,
                model_selector: report.run.map(|run| run.input.model),
                model_source: None,
                resource_binding: None,
            });
        }
    }
    for source in [
        "native_worker",
        "native_acp_attachment",
        "managed_core_channel",
        "managed_native_session",
    ] {
        coverage.push(Coverage {
            source: source.into(), status: if native.is_some() { "observed" } else { "unavailable" }.into(),
            detail: match source {
                "native_worker" => "Resident TurnWorker guards across daemon callers; cold/loaded Threads and queued Turns excluded",
                "native_acp_attachment" => "Live session attachments only; initialized connections without a session are outside this view",
                "managed_native_session" => "Daemon-owned NativeSession drivers and cleanup; released or lost-owner journals excluded",
                _ => "Daemon-owned managed Core channel bindings; disconnected retained sessions excluded; harness execution is separately owned",
            }.into(),
        });
    }
    if let Some(supervisor) = supervisor {
        for run in supervisor.connected_runs().await {
            rows.push(Instance {
                kind: Kind::SupervisedAcp,
                id: run.run_id.clone(),
                agent: run.agent_id,
                state: match (run.process, run.turn) {
                    (crate::supervisor::ProcessState::Stopping, _) => "stopping",
                    (_, crate::supervisor::TurnState::Idle) => "idle",
                    (_, crate::supervisor::TurnState::Submitting) => "submitting",
                    (_, crate::supervisor::TurnState::Working) => "working",
                    (_, crate::supervisor::TurnState::Cancelling) => "cancelling",
                }
                .into(),
                thread_id: None,
                turn_id: None,
                run_id: Some(run.run_id),
                session_id: run.native_session_id,
                agent_session_id: run.agent_session_id,
                via: run.via,
                connection_id: None,
                parent_run_id: run.parent_run_id,
                harness_id: None,
                execution_epoch: None,
                runtime_instance_id: None,
                model_source: run
                    .confirmed_route
                    .as_ref()
                    .map(|_| "confirmed_route".into()),
                model: run.confirmed_route,
                model_selector: None,
                resource_binding: None,
            });
        }
    }
    coverage.push(Coverage {
        source: "supervised_acp".into(), status: if supervisor.is_some() { "observed" } else { "unavailable" }.into(),
        detail: "Initialized live controller and unreaped process; idle/detached included; retained runs and leases excluded".into(),
    });
    for (source, detail) in [
        (
            "embedded_managed_native",
            "Independent library embeddings are outside this daemon; product task managed uses managed_native_session",
        ),
        (
            "independent_acp_controller",
            "Owned by an independent CLI process; route leases do not prove connection liveness",
        ),
        (
            "native_launcher",
            "External harness owns its UI/process; no daemon-owned connection proof",
        ),
        (
            "inference_only",
            "Router request traffic does not identify a live agent instance",
        ),
    ] {
        coverage.push(Coverage {
            source: source.into(),
            status: "not_observable".into(),
            detail: detail.into(),
        });
    }
    rows.sort_by_key(Instance::key);
    rows.retain(|row| after_key.is_none_or(|key| row.key().as_str() > key));
    let next_cursor = if rows.len() > limit {
        rows.get(limit - 1)
            .map(|row| format!("{instance}/{}", row.key()))
    } else {
        None
    };
    rows.truncate(limit);
    Ok(Report {
        daemon_state: "running".into(),
        server_instance_id: Some(instance.into()),
        sampled_at: Some(chrono::Utc::now()),
        resolved_via: "owner_local_control".into(),
        coverage,
        instances: (native.is_some() || supervisor.is_some()).then_some(rows),
        next_cursor,
        limit,
    })
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

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn missing_inspection_owners_are_unknown_and_invalid_pages_are_rejected() -> Result<()> {
        let report = snapshot("boot", None, None, None, MAX_PAGE).await?;
        assert_eq!(report.daemon_state, "running");
        assert!(report.instances.is_none());
        assert!(
            report
                .coverage
                .iter()
                .all(|source| source.status != "observed")
        );
        for limit in [0, MAX_PAGE + 1] {
            assert!(snapshot("boot", None, None, None, limit).await.is_err());
        }
        assert!(
            snapshot("boot", None, None, Some("old/native_worker:turn"), 1)
                .await
                .is_err()
        );
        Ok(())
    }
}
