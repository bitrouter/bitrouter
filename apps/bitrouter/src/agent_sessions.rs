//! Clients for daemon-supervised ACP runs.
//!
//! The daemon owns controllers, leases, the minimal ledger, and retained live
//! events. This module only resolves the local endpoint, sends the versioned
//! typed protocol, and returns replies to CLI and embedding callers.

use std::collections::{BTreeSet, HashMap};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Result, bail};
use tokio::sync::Mutex;

use crate::client::sessions::wire::{
    ActionAcknowledgement, ControlFence, LeaseMode, RunAttachment, RunSnapshot, RunSummary,
    SessionCommand, SessionGrant, SessionResponse, SessionScope, StartRunRequest,
};
use crate::paths::ConfigSource;

pub mod control;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Accepted supervised start plus non-fatal routing/launch diagnostics for the
/// caller to report without the protocol client writing output.
#[derive(Debug)]
pub struct StartedRun {
    pub snapshot: RunSnapshot,
    pub diagnostics: Vec<String>,
}

#[derive(Debug, Clone)]
struct HeldLease {
    generation: u64,
    heartbeat_at: Instant,
}

#[derive(Debug, Clone)]
struct AttachedRun {
    run_id: String,
}

#[derive(Debug, Default)]
struct ClientState {
    leases: HashMap<String, HeldLease>,
    attached: Option<AttachedRun>,
}

/// One local client identity and daemon endpoint. Clones intentionally share
/// the identity so an async effect handler and its snapshot poller are one
/// lease owner.
#[derive(Clone, Debug)]
pub struct BackgroundClient {
    socket: PathBuf,
    client_id: String,
    scopes: BTreeSet<SessionScope>,
    grant: Arc<Mutex<Option<SessionGrant>>>,
    state: Arc<Mutex<ClientState>>,
    attachment_gate: Arc<Mutex<()>>,
}

impl BackgroundClient {
    pub fn new(socket: PathBuf) -> Self {
        Self::with_scopes(
            socket,
            [
                SessionScope::Start,
                SessionScope::Peek,
                SessionScope::Transcript,
                SessionScope::Attach,
                SessionScope::Respond,
                SessionScope::Stop,
            ],
        )
    }

    fn start_only(socket: PathBuf) -> Self {
        Self::with_scopes(socket, [SessionScope::Start])
    }

    pub fn list_only(socket: PathBuf) -> Self {
        Self::with_scopes(socket, [SessionScope::List])
    }

    pub fn stop_only(socket: PathBuf) -> Self {
        Self::with_scopes(socket, [SessionScope::Stop])
    }

    pub fn remove_only(socket: PathBuf) -> Self {
        Self::with_scopes(socket, [SessionScope::Remove])
    }

    fn with_scopes(socket: PathBuf, scopes: impl IntoIterator<Item = SessionScope>) -> Self {
        Self {
            socket,
            client_id: format!("bro-{}", uuid::Uuid::new_v4()),
            scopes: scopes.into_iter().collect(),
            grant: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(ClientState::default())),
            attachment_gate: Arc::new(Mutex::new(())),
        }
    }

    pub fn socket(&self) -> &Path {
        &self.socket
    }

    pub fn client_id(&self) -> &str {
        &self.client_id
    }

    /// Replace-style snapshot for a control deck. `foreground_run_id` is
    /// omitted from the background inventory rather than relabelled.
    pub async fn runs(&self, foreground_run_id: Option<&str>) -> Result<Vec<RunSnapshot>> {
        match self.request(SessionCommand::PeekAll).await? {
            SessionResponse::Runs { mut runs } => {
                if let Some(foreground) = foreground_run_id {
                    runs.retain(|run| run.run_id != foreground);
                }
                Ok(runs)
            }
            response => unexpected("session list", response),
        }
    }

    /// Metadata-only inventory for scripts. This grant cannot read retained
    /// transcript, permission context/options, failures, results, or tools.
    pub async fn summaries(&self) -> Result<Vec<RunSummary>> {
        match self.request(SessionCommand::List).await? {
            SessionResponse::RunSummaries { runs } => Ok(runs),
            response => unexpected("session summary list", response),
        }
    }

    pub async fn start(&self, mut start: StartRunRequest) -> Result<StartedRun> {
        start.client_id = Some(self.client_id.clone());
        match self.request(SessionCommand::Start(Box::new(start))).await? {
            SessionResponse::Started {
                snapshot,
                diagnostics,
            } => Ok(StartedRun {
                snapshot: *snapshot,
                diagnostics,
            }),
            response => unexpected("background start", response),
        }
    }

    pub async fn attach(&self, run_id: &str, takeover: bool) -> Result<RunAttachment> {
        let _attachment_guard = self.attachment_gate.lock().await;
        self.attach_unlocked(run_id, takeover).await
    }

    async fn attach_unlocked(&self, run_id: &str, takeover: bool) -> Result<RunAttachment> {
        match self
            .request(SessionCommand::Attach {
                run_id: run_id.to_string(),
                client_id: self.client_id.clone(),
                after_seq: None,
                action_request_id: action_request_id(),
                takeover,
            })
            .await?
        {
            SessionResponse::Attachment { attachment } => {
                let attachment = *attachment;
                let mut state = self.state.lock().await;
                if let Some(lease) = attachment.snapshot.lease.as_ref() {
                    state.leases.insert(
                        run_id.to_string(),
                        HeldLease {
                            generation: lease.generation,
                            heartbeat_at: Instant::now(),
                        },
                    );
                }
                state.attached = Some(AttachedRun {
                    run_id: run_id.to_string(),
                });
                Ok(attachment)
            }
            response => unexpected("session attach", response),
        }
    }

    /// Stop is an explicit CLI confirmation. It acquires a transient lease
    /// but never steals another client's lease.
    pub async fn stop(&self, run_id: &str) -> Result<ActionAcknowledgement> {
        let lease = match self
            .request(SessionCommand::AcquireLease {
                run_id: run_id.to_string(),
                client_id: self.client_id.clone(),
                mode: LeaseMode::Transient,
                action_request_id: action_request_id(),
                takeover: false,
            })
            .await?
        {
            SessionResponse::Lease { lease } => lease,
            response => return unexpected("stop lease", response),
        };
        let fence = ControlFence {
            run_id: run_id.to_string(),
            client_id: self.client_id.clone(),
            lease_generation: lease.generation,
            action_request_id: action_request_id(),
        };
        let response = self
            .request(SessionCommand::Stop {
                fence: fence.clone(),
                confirmed: true,
            })
            .await;
        if response.is_err() {
            let _ = self
                .request(SessionCommand::ReleaseLease {
                    fence: ControlFence {
                        action_request_id: action_request_id(),
                        ..fence
                    },
                })
                .await;
        }
        match response? {
            SessionResponse::Action { acknowledgement } => {
                let mut state = self.state.lock().await;
                state.leases.remove(run_id);
                if state
                    .attached
                    .as_ref()
                    .is_some_and(|run| run.run_id == run_id)
                {
                    state.attached = None;
                }
                Ok(*acknowledgement)
            }
            response => unexpected("session stop", response),
        }
    }

    pub async fn remove(&self, run_id: &str) -> Result<String> {
        match self
            .request(SessionCommand::Remove {
                run_id: run_id.to_string(),
                action_request_id: action_request_id(),
            })
            .await?
        {
            SessionResponse::Removed { run_id } => Ok(run_id),
            response => unexpected("session remove", response),
        }
    }

    /// Release every client-owned lease on a normal UI exit. This never stops
    /// a run; an unexpected client loss remains covered by supervisor expiry.
    pub async fn release_all(&self) {
        let _attachment_guard = self.attachment_gate.lock().await;
        let held = {
            let mut state = self.state.lock().await;
            state.attached = None;
            std::mem::take(&mut state.leases)
        };
        for (run_id, lease) in held {
            let _ = self
                .request(SessionCommand::ReleaseLease {
                    fence: ControlFence {
                        run_id,
                        client_id: self.client_id.clone(),
                        lease_generation: lease.generation,
                        action_request_id: action_request_id(),
                    },
                })
                .await;
        }
    }

    /// Renew only the leases already held by this protocol client.
    pub async fn heartbeat_due_leases(&self) -> Result<()> {
        let due = {
            let mut state = self.state.lock().await;
            let now = Instant::now();
            state
                .leases
                .iter_mut()
                .filter_map(|(run_id, lease)| {
                    (now.duration_since(lease.heartbeat_at) >= HEARTBEAT_INTERVAL).then(|| {
                        lease.heartbeat_at = now;
                        (run_id.clone(), lease.generation)
                    })
                })
                .collect::<Vec<_>>()
        };
        for (run_id, generation) in due {
            let response = self
                .request(SessionCommand::Heartbeat {
                    run_id: run_id.clone(),
                    client_id: self.client_id.clone(),
                    lease_generation: generation,
                })
                .await;
            match response {
                Ok(SessionResponse::Lease { .. }) => {}
                Ok(response) => return unexpected("lease heartbeat", response),
                Err(error) => {
                    self.state.lock().await.leases.remove(&run_id);
                    return Err(error);
                }
            }
        }
        Ok(())
    }

    async fn request(&self, command: SessionCommand) -> Result<SessionResponse> {
        let grant = self.grant().await?;
        crate::client::sessions::request(&self.socket, &grant, command).await
    }

    async fn grant(&self) -> Result<SessionGrant> {
        if let Some(grant) = self.grant.lock().await.clone() {
            return Ok(grant);
        }
        let grant =
            crate::client::sessions::authorize(&self.socket, &self.client_id, self.scopes.clone())
                .await?;
        anyhow::ensure!(
            grant.client_id == self.client_id && grant.scopes == self.scopes,
            "daemon returned a session grant with a different identity or scope"
        );
        *self.grant.lock().await = Some(grant.clone());
        Ok(grant)
    }
}

/// Ensure the selected daemon owns the supervisor before accepting a run.
pub async fn start_background(
    source: &ConfigSource,
    config: &crate::config::Config,
    no_start: bool,
    request: StartRunRequest,
) -> Result<RunSnapshot> {
    let socket = crate::actions::supervised::ensure_daemon(source, config, no_start).await?;
    let started = BackgroundClient::start_only(socket).start(request).await?;
    for diagnostic in started.diagnostics {
        eprintln!("{diagnostic}");
    }
    Ok(started.snapshot)
}

fn action_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn unexpected<T>(operation: &str, _: SessionResponse) -> Result<T> {
    bail!("daemon returned an unexpected response to {operation}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_clones_keep_one_lease_identity() {
        let client = BackgroundClient::new("bitrouter.sock".into());
        let clone = client.clone();
        assert_eq!(client.client_id(), clone.client_id());
        assert_eq!(client.socket(), clone.socket());
    }

    #[test]
    fn client_profiles_request_only_their_declared_session_scopes() {
        let socket = "bitrouter.sock".into();
        assert_eq!(
            BackgroundClient::new("bitrouter.sock".into()).scopes,
            [
                SessionScope::Start,
                SessionScope::Peek,
                SessionScope::Transcript,
                SessionScope::Attach,
                SessionScope::Respond,
                SessionScope::Stop,
            ]
            .into_iter()
            .collect()
        );
        assert_eq!(
            BackgroundClient::list_only(socket).scopes,
            [SessionScope::List].into_iter().collect()
        );
        assert_eq!(
            BackgroundClient::stop_only("bitrouter.sock".into()).scopes,
            [SessionScope::Stop].into_iter().collect()
        );
        assert_eq!(
            BackgroundClient::remove_only("bitrouter.sock".into()).scopes,
            [SessionScope::Remove].into_iter().collect()
        );
        assert_eq!(
            BackgroundClient::start_only("bitrouter.sock".into()).scopes,
            [SessionScope::Start].into_iter().collect()
        );
    }
}
