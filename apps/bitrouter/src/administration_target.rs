//! Typed local-or-remote inspection ports.
//!
//! A remote context names the complete target.  This module resolves that
//! choice before a caller can load a local config, socket, database, or
//! provider environment, then exposes the same read operations to the CLI and
//! operations dashboard.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use anyhow::Result;
use bitrouter_mcp::actions::models::ModelsReport;
use bitrouter_mcp::actions::route::{RouteInput, RouteReport};
use bitrouter_mcp::actions::status::StatusReport;

use crate::actions::administration::{
    AgentsReport, ObserveReport, PolicyInput, PolicyReport, PolicyView, ProvidersReport,
};
use crate::actions::models::RoutableModels;
use crate::actions::requests::{RequestFilters, RequestsAction};
use crate::actions::route::RouteAction;
use crate::actions::status::DaemonStatus;
use crate::contexts::RemoteContext;
use crate::daemon::{
    self, DaemonCommand, DaemonInspection, DaemonInspectionReport, DaemonResponse,
};
use crate::output::reports::requests::RequestsReport;
use crate::paths::ConfigSource;
use crate::reload::ReloadState;
use crate::remote_control::operations::{OperationReport, ReloadInput};

/// Result of an explicit reload submitted through a selected target.
///
/// A local control socket completes synchronously and can only return the
/// coordinator's current state. Remote control retains an exact operation
/// receipt by request id, including a still-running operation after bounded
/// client polling.
pub enum ReloadSubmission {
    Local(ReloadState),
    Remote(OperationReport),
}

/// The currently advertised control grants relevant to the dashboard.
///
/// Server-side authorization remains authoritative when an action is sent. The
/// UI uses this snapshot only to avoid presenting a reload trigger to a
/// credential that discovery says cannot submit one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ControlAuthority {
    pub read: bool,
    pub reload: bool,
}

/// One selected target for a passive administration action.
#[derive(Clone)]
pub enum InspectionTarget {
    Local {
        source: ConfigSource,
        socket: PathBuf,
    },
    Remote {
        name: String,
        client: Arc<crate::remote_control::HttpControlClient>,
    },
}

impl InspectionTarget {
    /// Resolve the target once.  A remote context is complete by itself, so
    /// reject local flags before constructing a client or reading any local
    /// configuration.
    pub async fn resolve(
        remote: Option<(String, RemoteContext)>,
        config: Option<&Path>,
        socket: Option<&Path>,
    ) -> Result<Self> {
        match remote {
            Some((name, context)) => {
                reject_remote_local_flags(config, socket)?;
                Ok(Self::Remote {
                    name,
                    client: Arc::new(context.client()?),
                })
            }
            None => {
                let source = crate::paths::resolve_config(config)?;
                let socket = match socket {
                    Some(socket) => socket.to_path_buf(),
                    None => {
                        let config = crate::paths::load_config(&source).await?;
                        daemon::socket_path_for(&source, &config)
                    }
                };
                Ok(Self::Local { source, socket })
            }
        }
    }

    pub fn label(&self) -> String {
        match self {
            Self::Local { .. } => "local".to_string(),
            Self::Remote { name, .. } => format!("remote:{name}"),
        }
    }

    /// Local ACP sessions are deliberately absent from a remote inspection
    /// target.  The catalog itself remains readable through [`Self::agents`].
    pub fn allows_agent_launch(&self) -> bool {
        matches!(self, Self::Local { .. })
    }

    /// The dashboard uses this only to choose the explicitly initiated reload
    /// protocol. It does not widen the local socket protocol.
    pub fn is_remote(&self) -> bool {
        matches!(self, Self::Remote { .. })
    }

    pub fn local_source(&self) -> Option<&ConfigSource> {
        match self {
            Self::Local { source, .. } => Some(source),
            Self::Remote { .. } => None,
        }
    }

    /// The local-only adapter for legacy CLI output may retain the socket in
    /// its established JSON shape. Remote targets never expose a socket.
    pub fn local_socket(&self) -> Option<&Path> {
        match self {
            Self::Local { socket, .. } => Some(socket),
            Self::Remote { .. } => None,
        }
    }

    pub async fn status(&self) -> Result<StatusReport> {
        match self {
            Self::Local { source, socket } => {
                DaemonStatus::new(socket, Some(source.clone()))
                    .report()
                    .await
            }
            Self::Remote { client, .. } => client.status().await,
        }
    }

    pub async fn models(&self, provider: Option<&str>) -> Result<ModelsReport> {
        match self {
            Self::Local { source, socket } => {
                Ok(RoutableModels::new(source.clone(), Some(socket.clone()))
                    .report()
                    .await?
                    .filtered(provider))
            }
            Self::Remote { client, .. } => client.models(provider).await,
        }
    }

    pub async fn requests(&self, filters: RequestFilters) -> Result<RequestsReport> {
        match self {
            Self::Local { source, socket } => {
                RequestsAction::new(source.clone(), socket.clone())
                    .report_filtered(filters)
                    .await
            }
            Self::Remote { client, .. } => client.requests_filtered(&filters).await,
        }
    }

    pub async fn route(&self, input: RouteInput) -> Result<RouteReport> {
        match self {
            Self::Local { source, socket } => {
                RouteAction::new(source.clone(), Some(socket.clone()))
                    .report(input)
                    .await
            }
            Self::Remote { client, .. } => client.route(&input).await,
        }
    }

    pub async fn providers(&self) -> Result<ProvidersReport> {
        match self {
            Self::Local { .. } => match self.inspect(DaemonInspection::Providers).await? {
                DaemonInspectionReport::Providers(report) => Ok(report),
                other => unexpected_inspection("providers", other),
            },
            Self::Remote { client, .. } => client.providers().await,
        }
    }

    pub async fn agents(&self) -> Result<AgentsReport> {
        match self {
            Self::Local { .. } => match self.inspect(DaemonInspection::Agents).await? {
                DaemonInspectionReport::Agents(report) => Ok(report),
                other => unexpected_inspection("agents", other),
            },
            Self::Remote { client, .. } => client.agents().await,
        }
    }

    pub async fn observe(&self) -> Result<ObserveReport> {
        match self {
            Self::Local { .. } => match self.inspect(DaemonInspection::Observe).await {
                Ok(DaemonInspectionReport::Observe(report)) => Ok(report),
                Ok(other) => unexpected_inspection("observe", other),
                Err(error) if daemon::is_not_reachable(&error) => Ok(ObserveReport::from_snapshot(
                    daemon::ObserveStatusPayload::unwired(bitrouter_telemetry::OTEL_ENABLED),
                    false,
                )),
                Err(error) => Err(error),
            },
            Self::Remote { client, .. } => client.observe().await,
        }
    }

    pub async fn policy(&self, input: PolicyInput) -> Result<PolicyReport> {
        input.validate()?;
        match self {
            Self::Local { source, .. } if input.view == PolicyView::Disk => {
                crate::actions::administration::disk_policy(source)
                    .await?
                    .selected(input.name.as_deref())
            }
            Self::Local { .. } => match self.inspect(DaemonInspection::Policy { input }).await? {
                DaemonInspectionReport::Policy(report) => Ok(report),
                other => unexpected_inspection("policy", other),
            },
            Self::Remote { client, .. } => client.policy(&input).await,
        }
    }

    pub async fn reload_state(&self) -> Result<ReloadState> {
        match self {
            Self::Local { socket, .. } => {
                match daemon::send_command(socket, &DaemonCommand::ReloadState).await? {
                    DaemonResponse::ReloadState { state } => Ok(state),
                    DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
                    other => Err(anyhow::anyhow!(
                        "unexpected daemon reload-state response: {other:?}"
                    )),
                }
            }
            Self::Remote { client, .. } => client.state().await,
        }
    }

    /// Read the control discovery grants cached by the remote client. Local
    /// IPC is operator-local, so it has both capabilities when its daemon
    /// supports the corresponding command.
    pub async fn control_authority(&self) -> Result<ControlAuthority> {
        match self {
            Self::Local { .. } => Ok(ControlAuthority {
                read: true,
                reload: true,
            }),
            Self::Remote { client, .. } => Ok(ControlAuthority {
                read: client.can_action("status").await?,
                reload: client.can_action("reload").await?,
            }),
        }
    }

    pub async fn reload(&self) -> Result<ReloadSubmission> {
        match self {
            Self::Local { socket, .. } => {
                match daemon::send_command(socket, &DaemonCommand::Reload { env: Vec::new() })
                    .await?
                {
                    DaemonResponse::Ok => Ok(ReloadSubmission::Local(self.reload_state().await?)),
                    DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
                    other => Err(anyhow::anyhow!(
                        "unexpected daemon reload response: {other:?}"
                    )),
                }
            }
            Self::Remote { client, .. } => Ok(ReloadSubmission::Remote(client.reload().await?)),
        }
    }

    /// Submit an already fenced remote reload request. Keeping this separate
    /// from [`Self::reload`] lets the dashboard retain the generated request
    /// and instance identifiers if the HTTP response is interrupted.
    pub async fn submit_remote_reload(&self, input: &ReloadInput) -> Result<OperationReport> {
        match self {
            Self::Local { .. } => anyhow::bail!(
                "local reloads use the control socket and do not accept remote reload inputs"
            ),
            Self::Remote { client, .. } => client.submit_reload(input).await,
        }
    }

    pub async fn operation(&self, request_id: &str, instance: &str) -> Result<OperationReport> {
        match self {
            Self::Local { .. } => anyhow::bail!(
                "local reloads do not retain operation receipts; inspect the current reload state instead"
            ),
            Self::Remote { client, .. } => client.operation(request_id, instance).await,
        }
    }

    async fn inspect(&self, inspection: DaemonInspection) -> Result<DaemonInspectionReport> {
        let Self::Local { socket, .. } = self else {
            anyhow::bail!("local inspection is unavailable for a remote target");
        };
        match daemon::send_command(socket, &DaemonCommand::Inspect { inspection }).await? {
            DaemonResponse::Inspection { report } => Ok(report),
            DaemonResponse::Error { message } => Err(anyhow::anyhow!(message)),
            other => Err(anyhow::anyhow!(
                "unexpected daemon inspection response: {other:?}"
            )),
        }
    }
}

fn unexpected_inspection<T>(action: &str, report: DaemonInspectionReport) -> Result<T> {
    anyhow::bail!("daemon returned the wrong inspection report for {action}: {report:?}")
}

pub fn reject_remote_local_flags(config: Option<&Path>, socket: Option<&Path>) -> Result<()> {
    if config.is_some() || socket.is_some() {
        return Err(bitrouter_sdk::BitrouterError::bad_request(
            "a remote context does not accept --config or --socket; the named context is the \
             complete target",
        )
        .into());
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn remote_local_flags_are_rejected_before_the_token_is_read() -> anyhow::Result<()> {
        let config = tempfile::tempdir()?.path().join("bitrouter.yaml");
        let error = InspectionTarget::resolve(
            Some((
                "workstation".to_string(),
                RemoteContext {
                    endpoint: "https://router.example/control/v1/".to_string(),
                    token_env: "UNSET_TEST_CONTROL_TOKEN".to_string(),
                },
            )),
            Some(&config),
            None,
        )
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("remote config flag unexpectedly succeeded"))?;
        assert!(error.to_string().contains("does not accept --config"));
        Ok(())
    }
}
