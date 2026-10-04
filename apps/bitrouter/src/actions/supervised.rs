//! Local supervised session clients used by Code and the session CLI.
//!
//! Endpoint discovery and process startup live at this application boundary;
//! terminal reducers receive only typed session state and effects.

use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Result, bail, ensure};

use crate::acp_cli::{CapabilitySnapshot, LaunchOptions, RoutingOptions, SessionSelection};
use crate::daemon::{self, DaemonStartOutcome};
use crate::paths::ConfigSource;
use crate::supervisor::{
    ActionAcknowledgement, ActionValue, ControlFence, PermissionPolicy, Presentation, ReplayBatch,
    RunSnapshot, SessionAction, SessionCommand, SessionGrant, SessionMutation, SessionResponse,
    SessionScope, StartRunRequest,
};
use agent_client_protocol::schema::v1::{
    ListSessionsResponse, SessionConfigOptionValue, SetSessionConfigOptionResponse,
};
use bitrouter_sdk::acp::client::SessionInitialSettings;
use tokio_util::sync::CancellationToken;

/// Resolve the selected configuration's live endpoint, starting its daemon
/// only when the caller explicitly permits local startup.
pub(crate) async fn ensure_daemon(
    source: &ConfigSource,
    config: &bitrouter_sdk::config::Config,
    no_start: bool,
) -> Result<PathBuf> {
    if let Some(located) = crate::daemon_locator::locate_source(source).await? {
        crate::upgrade::ensure_compatible(source, located.socket(), no_start).await?;
        return Ok(located.socket().to_path_buf());
    }
    let socket = daemon::socket_path_for(source, config);
    if daemon::probe_status(&socket).await?.is_some() {
        crate::upgrade::ensure_compatible(source, &socket, no_start).await?;
        return Ok(socket);
    }
    ensure!(
        !no_start,
        "Supervised sessions require the selected local daemon; start it or omit --no-start"
    );
    let log = source.home().join("bitrouter.log");
    match daemon::start_and_wait(source, &log, Some(&socket), Duration::from_secs(15)).await? {
        DaemonStartOutcome::Ready(_) => Ok(socket),
        DaemonStartOutcome::Exited { status, log_tail } => {
            bail!("Session supervisor exited ({status}): {log_tail}")
        }
        DaemonStartOutcome::NotReadyInTime { pid } => {
            bail!(
                "Session supervisor process {pid} has not become ready; inspect {} before retrying",
                log.display()
            )
        }
    }
}

/// A session client has no process handle. Clones retain the same fenced
/// authority, so a delayed operation cannot acquire a newer lease implicitly.
#[derive(Clone)]
pub(crate) struct SupervisedClient {
    pub socket: PathBuf,
    pub grant: SessionGrant,
    pub run_id: String,
    pub client_id: String,
    pub generation: u64,
}

impl SupervisedClient {
    pub fn fence(&self) -> ControlFence {
        ControlFence {
            run_id: self.run_id.clone(),
            client_id: self.client_id.clone(),
            lease_generation: self.generation,
            action_request_id: uuid::Uuid::new_v4().to_string(),
        }
    }

    pub async fn mutate(&self, action: SessionAction) -> Result<ActionAcknowledgement> {
        let command = SessionCommand::Mutate(SessionMutation {
            fence: self.fence(),
            action,
        });
        match crate::supervisor::request(&self.socket, &self.grant, command).await? {
            SessionResponse::Action { acknowledgement } => Ok(*acknowledgement),
            _ => bail!("Unexpected supervised action response"),
        }
    }

    pub async fn events(&self, after_seq: u64) -> Result<ReplayBatch> {
        match crate::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::Events {
                run_id: self.run_id.clone(),
                after_seq,
            },
        )
        .await?
        {
            SessionResponse::Events { replay } => Ok(replay),
            _ => bail!("Unexpected supervised events response"),
        }
    }

    pub async fn snapshot(&self) -> Result<RunSnapshot> {
        match crate::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::Snapshot {
                run_id: self.run_id.clone(),
            },
        )
        .await?
        {
            SessionResponse::Snapshot { snapshot } => Ok(*snapshot),
            _ => bail!("Unexpected supervised snapshot response"),
        }
    }

    pub async fn heartbeat(&self) -> Result<()> {
        crate::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::Heartbeat {
                run_id: self.run_id.clone(),
                client_id: self.client_id.clone(),
                lease_generation: self.generation,
            },
        )
        .await?;
        Ok(())
    }

    pub async fn detach(&self) -> Result<()> {
        crate::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::ReleaseLease {
                fence: self.fence(),
            },
        )
        .await?;
        Ok(())
    }

    pub async fn stop(&self) -> Result<()> {
        crate::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::Stop {
                fence: self.fence(),
                confirmed: true,
            },
        )
        .await?;
        Ok(())
    }

    pub async fn list_sessions(
        &self,
        cwd: Option<PathBuf>,
        cursor: Option<String>,
    ) -> Result<ListSessionsResponse> {
        match crate::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::NativeList {
                run_id: self.run_id.clone(),
                cwd,
                cursor,
            },
        )
        .await?
        {
            SessionResponse::NativeSessions { response } => Ok(response),
            _ => bail!("Unexpected native session list response"),
        }
    }

    pub async fn set_session_mode(&self, _session: &str, mode_id: String) -> Result<()> {
        self.mutate(SessionAction::SetMode { mode_id }).await?;
        Ok(())
    }

    pub async fn set_session_config_option(
        &self,
        _session: &str,
        config_id: String,
        value: SessionConfigOptionValue,
    ) -> Result<SetSessionConfigOptionResponse> {
        let acknowledgement = self
            .mutate(SessionAction::SetConfig { config_id, value })
            .await?;
        match acknowledgement.value {
            Some(ActionValue::ConfigOptions { options }) => {
                Ok(SetSessionConfigOptionResponse::new(options))
            }
            _ => bail!("Supervisor omitted confirmed configuration options"),
        }
    }

    pub async fn route_list(&self, _session: &str) -> Result<crate::supervisor::RouteState> {
        match crate::supervisor::request(
            &self.socket,
            &self.grant,
            SessionCommand::RouteList {
                run_id: self.run_id.clone(),
            },
        )
        .await?
        {
            SessionResponse::RouteState { state } => Ok(state),
            _ => bail!("Unexpected supervised route response"),
        }
    }

    pub async fn route_set(&self, _session: &str, route: &str) -> Result<String> {
        let acknowledgement = self
            .mutate(SessionAction::RouteSet {
                route: route.into(),
            })
            .await?;
        match acknowledgement.value {
            Some(ActionValue::Route {
                current: Some(current),
            }) => Ok(current),
            _ => bail!("Supervisor omitted the confirmed session route"),
        }
    }

    pub async fn route_reset(&self, _session: &str) -> Result<()> {
        self.mutate(SessionAction::RouteClear).await?;
        Ok(())
    }
}

pub(crate) struct SupervisedHandle {
    pub client: SupervisedClient,
    pub session_id: String,
    pub agent_session_id: Option<String>,
    pub agent_id: String,
    pub via: Option<String>,
    pub capabilities: CapabilitySnapshot,
    pub initial_settings: SessionInitialSettings,
    pub last_seq: u64,
}

pub(crate) struct ForegroundLaunch {
    pub agent_id: String,
    pub cwd: PathBuf,
    pub routing: RoutingOptions,
    pub options: LaunchOptions,
    pub selection: SessionSelection,
}

impl SupervisedHandle {
    pub async fn start(socket: PathBuf, launch: ForegroundLaunch) -> Result<(Self, Vec<String>)> {
        let client_id = uuid::Uuid::new_v4().to_string();
        let grant = crate::supervisor::authorize(
            &socket,
            &client_id,
            [
                SessionScope::Start,
                SessionScope::Peek,
                SessionScope::Transcript,
                SessionScope::Attach,
                SessionScope::Respond,
                SessionScope::Stop,
            ]
            .into_iter()
            .collect(),
        )
        .await?;
        let request = StartRunRequest {
            action_request_id: uuid::Uuid::new_v4().to_string(),
            client_id: Some(client_id.clone()),
            label: None,
            agent_id: launch.agent_id,
            prompt: None,
            cwd: launch.cwd,
            routing: launch.routing,
            launch: LaunchOptions {
                terminal_auth: false,
                ..launch.options
            },
            session: launch.selection,
            presentation: Presentation::Foreground,
            parent_run_id: None,
            allow_shared_directory: false,
            permission_policy: PermissionPolicy::default(),
            result_schema: None,
        };
        let (snapshot, diagnostics) = match crate::supervisor::request(
            &socket,
            &grant,
            SessionCommand::Start(Box::new(request)),
        )
        .await?
        {
            SessionResponse::Started {
                snapshot,
                diagnostics,
            } => (*snapshot, diagnostics),
            _ => bail!("Supervisor did not acknowledge foreground ownership"),
        };
        let lease = snapshot
            .lease
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("Supervisor omitted foreground control lease"))?;
        ensure!(
            lease.owner_client_id == client_id,
            "Supervisor returned a different foreground owner"
        );
        let client = SupervisedClient {
            socket,
            grant,
            run_id: snapshot.run_id.clone(),
            client_id,
            generation: lease.generation,
        };
        let mut handle = Self {
            client,
            session_id: String::new(),
            agent_session_id: None,
            agent_id: String::new(),
            via: None,
            capabilities: CapabilitySnapshot::default(),
            initial_settings: SessionInitialSettings::default(),
            last_seq: 0,
        };
        handle.refresh(snapshot)?;
        // Startup/load notifications precede the response. The first consumer
        // reads them from sequence zero rather than losing that replay.
        handle.last_seq = 0;
        Ok((handle, diagnostics))
    }

    pub fn refresh(&mut self, snapshot: RunSnapshot) -> Result<()> {
        self.session_id = snapshot
            .native_session_id
            .ok_or_else(|| anyhow::anyhow!("Supervisor omitted native session identity"))?;
        self.agent_session_id = snapshot.agent_session_id;
        self.agent_id = snapshot.agent_id;
        self.via = snapshot.via;
        self.capabilities = snapshot
            .capabilities
            .ok_or_else(|| anyhow::anyhow!("Supervisor omitted negotiated capabilities"))?;
        self.initial_settings = snapshot.initial_settings.unwrap_or_default();
        self.last_seq = snapshot.last_seq;
        Ok(())
    }

    pub async fn select_with_cancel(
        &mut self,
        selection: &SessionSelection,
        cancel: &CancellationToken,
    ) -> Result<()> {
        if cancel.is_cancelled() {
            return Err(crate::acp_cli::lifecycle_cancelled());
        }
        // Once accepted, finish observing the operation even if the terminal
        // disappears. The caller will detach the resulting native session.
        let before = self.client.snapshot().await?.last_seq;
        let acknowledgement = self
            .client
            .mutate(SessionAction::SelectSession {
                selection: selection.clone(),
            })
            .await?;
        self.refresh(acknowledgement.snapshot)?;
        self.last_seq = before;
        Ok(())
    }
}
