//! Clients for daemon-supervised ACP runs.
//!
//! The daemon owns controllers, leases, the minimal ledger, and retained live
//! events. This module only resolves the local endpoint, sends the versioned
//! typed protocol, and adapts replies for CLI and TUI callers.

use std::collections::{BTreeSet, HashMap, VecDeque};
use std::io::IsTerminal as _;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{Context, Result, bail};
use bitrouter_tui::agents::{
    AgentAction, AgentAttention, AgentDeckMode, AgentDeckSnapshot, AgentDeckState, AgentDeckView,
    AgentEffect, AgentHistoryEvent, AgentHistoryKind, AgentHistorySnapshot, AgentLeaseIntent,
    AgentLeaseView, AgentPermissionOption, AgentPermissionView, AgentProcessState,
    AgentReviewState, AgentRunView, AgentTurnState, NewAgentRunTarget,
};
use bitrouter_tui::code::{CodeEffect, CodeState};
use crossterm::event::EventStream;
use futures::{FutureExt as _, StreamExt as _};
use tokio::sync::Mutex;
use tokio::time::MissedTickBehavior;

use crate::acp_cli::{RoutingOptions, SessionSelection};
use crate::paths::ConfigSource;
use crate::supervisor::{
    ActionAcknowledgement, AttentionState, ControlFence, LeaseMode, ProcessState, ReviewState,
    RunAttachment, RunSnapshot, RunSummary, SessionAction, SessionCommand, SessionEvent,
    SessionEventKind, SessionGrant, SessionMutation, SessionResponse, SessionScope,
    StartRunRequest, TurnState,
};
use bitrouter_sdk::acp::translate::SessionUpdateKind;

const HEARTBEAT_INTERVAL: Duration = Duration::from_secs(5);

/// Accepted supervised start plus non-fatal routing/launch diagnostics for the
/// presentation layer to render without the client writing into a TUI.
#[derive(Debug)]
pub struct StartedRun {
    pub snapshot: RunSnapshot,
    pub diagnostics: Vec<String>,
}

/// Presentation-neutral completion of one background client operation.
#[derive(Debug)]
pub enum AgentResult {
    Snapshot(AgentDeckSnapshot),
    History(Box<AgentHistorySnapshot>),
    HistoryEvent {
        run_id: String,
        event: AgentHistoryEvent,
    },
    Action(AgentAction),
    Diagnostics(Vec<String>),
    Batch(Vec<AgentResult>),
    Exit,
}

#[derive(Debug, Clone)]
struct HeldLease {
    generation: u64,
    mode: LeaseMode,
    heartbeat_at: Instant,
}

#[derive(Debug, Clone)]
struct AttachedRun {
    run_id: String,
    after_seq: u64,
}

#[derive(Debug, Default)]
struct ClientState {
    leases: HashMap<String, HeldLease>,
    attached: Option<AttachedRun>,
    foreground_run_id: Option<String>,
    next_snapshot_sequence: u64,
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
    operation_gate: Arc<Mutex<()>>,
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

    fn attach_interactive(socket: PathBuf) -> Self {
        Self::with_scopes(
            socket,
            [
                SessionScope::Peek,
                SessionScope::Transcript,
                SessionScope::Attach,
                SessionScope::Respond,
                SessionScope::Stop,
            ],
        )
    }

    fn with_scopes(socket: PathBuf, scopes: impl IntoIterator<Item = SessionScope>) -> Self {
        Self {
            socket,
            client_id: format!("bro-{}", uuid::Uuid::new_v4()),
            scopes: scopes.into_iter().collect(),
            grant: Arc::new(Mutex::new(None)),
            state: Arc::new(Mutex::new(ClientState::default())),
            operation_gate: Arc::new(Mutex::new(())),
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

    /// One replace-style list snapshot plus attached history deltas and lease
    /// heartbeats. Callers may poll this while a foreground job remains live;
    /// it never borrows their reducer across an await.
    pub async fn snapshot(
        &self,
        foreground_run_id: Option<&str>,
        new_run_target: Option<NewAgentRunTarget>,
    ) -> Result<AgentResult> {
        let _operation_guard = self.operation_gate.lock().await;
        self.snapshot_unlocked(foreground_run_id, new_run_target)
            .await
    }

    async fn snapshot_unlocked(
        &self,
        foreground_run_id: Option<&str>,
        new_run_target: Option<NewAgentRunTarget>,
    ) -> Result<AgentResult> {
        self.state.lock().await.foreground_run_id = foreground_run_id.map(ToString::to_string);
        let sequence = self.snapshot_ticket().await;
        self.heartbeat_due_leases().await?;
        let runs = self.runs(foreground_run_id).await?;
        let mut results = vec![AgentResult::Snapshot(map_deck_snapshot(
            runs,
            &self.client_id,
            new_run_target,
            sequence,
        ))];
        let _attachment_guard = self.attachment_gate.lock().await;
        let attached = self.state.lock().await.attached.clone();
        if let Some(attached) = attached {
            let replay = match self
                .request(SessionCommand::Events {
                    run_id: attached.run_id.clone(),
                    after_seq: attached.after_seq,
                })
                .await?
            {
                SessionResponse::Events { replay } => replay,
                response => return unexpected("attached session events", response),
            };
            if !self
                .state
                .lock()
                .await
                .attached
                .as_ref()
                .is_some_and(|current| current.run_id == attached.run_id)
            {
                return Ok(AgentResult::Batch(results));
            }
            if replay.first_retained_seq > attached.after_seq.saturating_add(1) {
                let attachment = self.attach_unlocked(&attached.run_id, false).await?;
                results.push(AgentResult::History(Box::new(map_history_snapshot(
                    attachment,
                    &self.client_id,
                ))));
                return Ok(AgentResult::Batch(results));
            }
            let next_seq = replay.snapshot_seq.max(attached.after_seq);
            let still_attached = {
                let mut state = self.state.lock().await;
                if let Some(current) = state
                    .attached
                    .as_mut()
                    .filter(|current| current.run_id == attached.run_id)
                {
                    current.after_seq = next_seq;
                    true
                } else {
                    false
                }
            };
            if still_attached {
                results.extend(
                    replay
                        .events
                        .iter()
                        .filter(|event| event.seq > attached.after_seq)
                        .map(map_history_event)
                        .map(|event| AgentResult::HistoryEvent {
                            run_id: attached.run_id.clone(),
                            event,
                        }),
                );
            }
        }
        Ok(AgentResult::Batch(results))
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
                            mode: LeaseMode::Attached,
                            heartbeat_at: Instant::now(),
                        },
                    );
                }
                state.attached = Some(AttachedRun {
                    run_id: run_id.to_string(),
                    after_seq: attachment.replay.snapshot_seq,
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

    /// Execute a typed reducer effect. Errors with a stable action id are
    /// returned to the reducer so target-bound drafts stay recoverable and its
    /// pending state cannot freeze.
    pub async fn handle_effect(
        &self,
        effect: AgentEffect,
        new_run_target: Option<NewAgentRunTarget>,
    ) -> Result<AgentResult> {
        let _operation_guard = if matches!(
            effect,
            AgentEffect::Copy { .. } | AgentEffect::Export { .. }
        ) {
            None
        } else {
            Some(self.operation_gate.lock().await)
        };
        let action_request_id = effect_action_request_id(&effect);
        let error_run_id = effect_transient_run_id(&effect);
        match self.handle_effect_inner(effect, new_run_target).await {
            Ok(result) => Ok(result),
            Err(error) => match action_request_id {
                Some(action_request_id) => {
                    if let Some(run_id) = error_run_id {
                        self.release_transient_after_error(&run_id).await;
                    }
                    Ok(AgentResult::Action(AgentAction::EffectFailed {
                        action_request_id,
                        message: format!("{error:#}"),
                    }))
                }
                None => Err(error),
            },
        }
    }

    /// Reject an effect that was queued before a foreground permission became
    /// pending. Any transient lease acquired for the action is released, and
    /// the original request id is returned to the reducer so pending UI state
    /// and target-bound drafts recover normally.
    pub async fn reject_queued_effect(
        &self,
        effect: &AgentEffect,
        message: impl Into<String>,
    ) -> AgentResult {
        let _operation_guard = self.operation_gate.lock().await;
        if let Some(run_id) = effect_transient_run_id(effect) {
            self.release_transient_after_error(&run_id).await;
        }
        match effect.action_request_id() {
            Some(action_request_id) => AgentResult::Action(AgentAction::EffectFailed {
                action_request_id: action_request_id.to_string(),
                message: message.into(),
            }),
            None => AgentResult::Diagnostics(vec![message.into()]),
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

    async fn handle_effect_inner(
        &self,
        effect: AgentEffect,
        new_run_target: Option<NewAgentRunTarget>,
    ) -> Result<AgentResult> {
        match effect {
            AgentEffect::Refresh => {
                let foreground = self.foreground_run_id().await;
                self.snapshot_unlocked(foreground.as_deref(), new_run_target)
                    .await
            }
            AgentEffect::AcquireLease {
                run_id,
                intent,
                action_request_id,
            } => {
                let mode = if intent == AgentLeaseIntent::Attach {
                    LeaseMode::Attached
                } else {
                    LeaseMode::Transient
                };
                let lease = match self
                    .request(SessionCommand::AcquireLease {
                        run_id: run_id.clone(),
                        client_id: self.client_id.clone(),
                        mode,
                        action_request_id: action_request_id.clone(),
                        takeover: false,
                    })
                    .await?
                {
                    SessionResponse::Lease { lease } => lease,
                    response => return unexpected("lease acquisition", response),
                };
                self.state.lock().await.leases.insert(
                    run_id.clone(),
                    HeldLease {
                        generation: lease.generation,
                        mode,
                        heartbeat_at: Instant::now(),
                    },
                );
                Ok(AgentResult::Action(AgentAction::LeaseAcquired {
                    run_id,
                    intent,
                    generation: lease.generation,
                    action_request_id,
                }))
            }
            AgentEffect::ReleaseLease {
                run_id,
                lease_generation,
                action_request_id,
            }
            | AgentEffect::Detach {
                run_id,
                lease_generation,
                action_request_id,
            } => {
                self.release(&run_id, lease_generation, &action_request_id)
                    .await?;
                Ok(accepted(action_request_id))
            }
            AgentEffect::Reply {
                run_id,
                prompt,
                lease_generation,
                action_request_id,
            } => {
                self.mutate(
                    &run_id,
                    lease_generation,
                    &action_request_id,
                    SessionAction::Prompt { text: prompt },
                )
                .await?;
                Ok(accepted(action_request_id))
            }
            AgentEffect::RespondPermission {
                run_id,
                permission_id,
                option_id,
                lease_generation,
                action_request_id,
            } => {
                self.mutate(
                    &run_id,
                    lease_generation,
                    &action_request_id,
                    SessionAction::Permission {
                        permission_id,
                        option_id,
                    },
                )
                .await?;
                Ok(accepted(action_request_id))
            }
            AgentEffect::NewRun {
                target,
                prompt,
                action_request_id,
            } => {
                let started = self
                    .start(StartRunRequest {
                        action_request_id: action_request_id.clone(),
                        client_id: Some(self.client_id.clone()),
                        label: None,
                        agent_id: target.agent,
                        prompt: Some(prompt),
                        cwd: PathBuf::from(target.directory),
                        routing: RoutingOptions {
                            model: target.route,
                            ..RoutingOptions::default()
                        },
                        launch: crate::acp_cli::launch_options(None),
                        session: SessionSelection::New,
                        presentation: crate::supervisor::Presentation::Background,
                        parent_run_id: None,
                        allow_shared_directory: false,
                        permission_policy: crate::supervisor::PermissionPolicy::default(),
                        result_schema: None,
                    })
                    .await?;
                let sequence = self.snapshot_ticket().await;
                let foreground = self.foreground_run_id().await;
                Ok(AgentResult::Batch(vec![
                    accepted(action_request_id),
                    AgentResult::Snapshot(map_deck_snapshot(
                        self.runs(foreground.as_deref()).await?,
                        &self.client_id,
                        new_run_target,
                        sequence,
                    )),
                    diagnostics_result(started.diagnostics),
                ]))
            }
            AgentEffect::CancelTurn {
                run_id,
                lease_generation,
                action_request_id,
            } => {
                self.mutate(
                    &run_id,
                    lease_generation,
                    &action_request_id,
                    SessionAction::Cancel,
                )
                .await?;
                Ok(accepted(action_request_id))
            }
            AgentEffect::MarkReviewed {
                run_id,
                lease_generation,
                action_request_id,
            } => {
                self.mutate(
                    &run_id,
                    lease_generation,
                    &action_request_id,
                    SessionAction::MarkReviewed,
                )
                .await?;
                Ok(accepted(action_request_id))
            }
            AgentEffect::Takeover {
                run_id,
                action_request_id,
            } => {
                let attachment = self.attach(&run_id, true).await?;
                Ok(AgentResult::Batch(vec![
                    AgentResult::History(Box::new(map_history_snapshot(
                        attachment,
                        &self.client_id,
                    ))),
                    accepted(action_request_id),
                ]))
            }
            AgentEffect::Stop {
                run_id,
                lease_generation,
                action_request_id,
            } => {
                self.stop_with_fence(&run_id, lease_generation, &action_request_id)
                    .await?;
                let sequence = self.snapshot_ticket().await;
                let foreground = self.foreground_run_id().await;
                Ok(AgentResult::Batch(vec![
                    AgentResult::Snapshot(map_deck_snapshot(
                        self.runs(foreground.as_deref()).await?,
                        &self.client_id,
                        new_run_target,
                        sequence,
                    )),
                    accepted(action_request_id),
                ]))
            }
            AgentEffect::Attach {
                run_id,
                action_request_id,
            } => {
                let attachment = self.attach(&run_id, false).await?;
                Ok(AgentResult::Batch(vec![
                    AgentResult::History(Box::new(map_history_snapshot(
                        attachment,
                        &self.client_id,
                    ))),
                    accepted(action_request_id),
                ]))
            }
            AgentEffect::Resync {
                run_id,
                expected_seq: _,
                received_seq: _,
            } => {
                let attachment = self.attach(&run_id, false).await?;
                Ok(AgentResult::History(Box::new(map_history_snapshot(
                    attachment,
                    &self.client_id,
                ))))
            }
            AgentEffect::Copy { text } => {
                crate::chat::editor::copy(&text).await?;
                Ok(AgentResult::Diagnostics(vec![
                    "Copied to clipboard".to_string(),
                ]))
            }
            AgentEffect::Export { run_id, content } => {
                let path = export_history(&run_id, &content).await?;
                Ok(AgentResult::Diagnostics(vec![format!(
                    "Exported agent history to {}",
                    path.display()
                )]))
            }
            AgentEffect::ExitStandalone => Ok(AgentResult::Exit),
        }
    }

    async fn heartbeat_due_leases(&self) -> Result<()> {
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

    async fn release_transient_after_error(&self, run_id: &str) {
        let lease = self.state.lock().await.leases.get(run_id).cloned();
        if let Some(lease) = lease
            && lease.mode == LeaseMode::Transient
        {
            let _ = self
                .release(run_id, lease.generation, &action_request_id())
                .await;
        }
    }

    async fn release(
        &self,
        run_id: &str,
        lease_generation: u64,
        action_request_id: &str,
    ) -> Result<ActionAcknowledgement> {
        let _attachment_guard = self.attachment_gate.lock().await;
        let acknowledgement = match self
            .request(SessionCommand::ReleaseLease {
                fence: ControlFence {
                    run_id: run_id.to_string(),
                    client_id: self.client_id.clone(),
                    lease_generation,
                    action_request_id: action_request_id.to_string(),
                },
            })
            .await?
        {
            SessionResponse::Action { acknowledgement } => *acknowledgement,
            response => return unexpected("lease release", response),
        };
        let mut state = self.state.lock().await;
        state.leases.remove(run_id);
        if state
            .attached
            .as_ref()
            .is_some_and(|run| run.run_id == run_id)
        {
            state.attached = None;
        }
        Ok(acknowledgement)
    }

    async fn mutate(
        &self,
        run_id: &str,
        lease_generation: u64,
        request_id: &str,
        action: SessionAction,
    ) -> Result<ActionAcknowledgement> {
        let acknowledgement = match self
            .request(SessionCommand::Mutate(SessionMutation {
                fence: ControlFence {
                    run_id: run_id.to_string(),
                    client_id: self.client_id.clone(),
                    lease_generation,
                    action_request_id: request_id.to_string(),
                },
                action,
            }))
            .await?
        {
            SessionResponse::Action { acknowledgement } => *acknowledgement,
            response => return unexpected("session mutation", response),
        };
        let mut state = self.state.lock().await;
        if state.leases.get(run_id).is_some_and(|lease| {
            lease.generation == lease_generation && lease.mode == LeaseMode::Transient
        }) {
            state.leases.remove(run_id);
        }
        Ok(acknowledgement)
    }

    async fn stop_with_fence(
        &self,
        run_id: &str,
        lease_generation: u64,
        action_request_id: &str,
    ) -> Result<ActionAcknowledgement> {
        let _attachment_guard = self.attachment_gate.lock().await;
        let acknowledgement = match self
            .request(SessionCommand::Stop {
                fence: ControlFence {
                    run_id: run_id.to_string(),
                    client_id: self.client_id.clone(),
                    lease_generation,
                    action_request_id: action_request_id.to_string(),
                },
                confirmed: true,
            })
            .await?
        {
            SessionResponse::Action { acknowledgement } => *acknowledgement,
            response => return unexpected("session stop", response),
        };
        let mut state = self.state.lock().await;
        state.leases.remove(run_id);
        if state
            .attached
            .as_ref()
            .is_some_and(|run| run.run_id == run_id)
        {
            state.attached = None;
        }
        Ok(acknowledgement)
    }

    async fn request(&self, command: SessionCommand) -> Result<SessionResponse> {
        let grant = self.grant().await?;
        crate::supervisor::request(&self.socket, &grant, command).await
    }

    async fn grant(&self) -> Result<SessionGrant> {
        if let Some(grant) = self.grant.lock().await.clone() {
            return Ok(grant);
        }
        let grant =
            crate::supervisor::authorize(&self.socket, &self.client_id, self.scopes.clone())
                .await?;
        anyhow::ensure!(
            grant.client_id == self.client_id && grant.scopes == self.scopes,
            "daemon returned a session grant with a different identity or scope"
        );
        *self.grant.lock().await = Some(grant.clone());
        Ok(grant)
    }

    async fn snapshot_ticket(&self) -> u64 {
        let mut state = self.state.lock().await;
        state.next_snapshot_sequence = state.next_snapshot_sequence.saturating_add(1);
        state.next_snapshot_sequence
    }

    async fn foreground_run_id(&self) -> Option<String> {
        self.state.lock().await.foreground_run_id.clone()
    }
}

/// Ensure the selected daemon owns the supervisor before accepting a run.
pub async fn start_background(
    source: &ConfigSource,
    config: &bitrouter_sdk::config::Config,
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

/// Open the local standalone control deck using the normal resolved config.
pub async fn run_standalone() -> Result<()> {
    ensure_interactive(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )?;
    let source = crate::paths::resolve_config(None)?;
    let config = crate::paths::load_config(&source).await?;
    let socket = crate::actions::supervised::ensure_daemon(&source, &config, false).await?;
    let client = BackgroundClient::new(socket);
    let target = match config.chat.agent.as_deref() {
        Some(agent) => Some(NewAgentRunTarget {
            agent: crate::acp_cli::resolve_agent_id(&config, agent)?,
            directory: std::env::current_dir()
                .context("resolving the agent working directory")?
                .display()
                .to_string(),
            route: config.chat.model.clone(),
            conflict: None,
        }),
        None => None,
    };
    let mut state = AgentDeckState::new(AgentDeckMode::Standalone, client.client_id());
    state.set_new_run_choices(
        crate::actions::code::background_choices(
            &config,
            target.as_ref().map(|target| target.agent.as_str()),
            false,
        )
        .await?,
    );
    let initial = client.snapshot(None, target.clone()).await?;
    let _ = apply_result(&mut state, initial);
    run_deck(client, state, target).await
}

/// Attach the standalone inspector to one daemon-owned run.
pub async fn run_attached(socket: PathBuf, run_id: &str) -> Result<()> {
    ensure_interactive(
        std::io::stdin().is_terminal(),
        std::io::stdout().is_terminal(),
    )?;
    let client = BackgroundClient::attach_interactive(socket);
    let mut state = AgentDeckState::new(AgentDeckMode::Standalone, client.client_id());
    let snapshot = client.snapshot(None, None).await?;
    let _ = apply_result(&mut state, snapshot);
    let attachment = client.attach(run_id, false).await?;
    state.expect_inspector_history(run_id);
    let _ = apply_result(
        &mut state,
        AgentResult::History(Box::new(map_history_snapshot(
            attachment,
            client.client_id(),
        ))),
    );
    run_deck(client, state, None).await
}

async fn run_deck(
    client: BackgroundClient,
    mut state: AgentDeckState,
    new_run_target: Option<NewAgentRunTarget>,
) -> Result<()> {
    let mut view = AgentDeckView::open().context("opening the agent manager")?;
    let mut events = Some(EventStream::new());
    let mut shutdown = crate::chat::signals::Shutdown::install();
    let mut refresh = tokio::time::interval(Duration::from_millis(500));
    refresh.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut pending = VecDeque::new();
    let result = async {
        // Keep in-flight work inside this scope so a terminal signal cancels
        // it before exit cleanup tries to acquire the same client gates.
        let mut effect_job = None;
        let mut snapshot_job = None;
        loop {
            view.draw(&state, false)?;
            if effect_job.is_none()
                && snapshot_job.is_none()
                && let Some(effect) = pending.pop_front()
            {
                if effect == AgentEffect::ExitStandalone {
                    break;
                }
                let effect_client = client.clone();
                let target = new_run_target.clone();
                effect_job = Some(
                    async move { effect_client.handle_effect(effect, target).await }.boxed_local(),
                );
            }
            tokio::select! {
                result = next_deck_job(&mut effect_job) => {
                    effect_job = None;
                    match result {
                        Ok(result) => {
                            if result_requests_exit(&result) {
                                break;
                            }
                            pending.extend(apply_result(&mut state, result));
                        }
                        Err(error) => show_deck_error(&mut state, error),
                    }
                }
                result = next_deck_job(&mut snapshot_job) => {
                    snapshot_job = None;
                    match result {
                        Ok(result) => pending.extend(apply_result(&mut state, result)),
                        Err(error) => show_deck_error(&mut state, error),
                    }
                }
                event = next_deck_input(&mut events) => {
                    let event = event
                        .ok_or_else(|| anyhow::anyhow!("terminal input ended"))??;
                    pending.extend(state.step(AgentAction::Event(event), false));
                }
                _ = refresh.tick() => {
                    if snapshot_job.is_none() && effect_job.is_none() && pending.is_empty() {
                        let snapshot_client = client.clone();
                        let target = new_run_target.clone();
                        snapshot_job = Some(
                            async move { snapshot_client.snapshot(None, target).await }.boxed_local(),
                        );
                    }
                }
                signal = shutdown.recv() => match signal {
                    crate::chat::signals::TerminalSignal::Shutdown => break,
                    crate::chat::signals::TerminalSignal::Suspend => {
                        drop(events.take());
                        view.finish()?;
                        crate::chat::signals::suspend_current_process()?;
                        view = AgentDeckView::open().context("resuming the agent manager")?;
                        events = Some(EventStream::new());
                    }
                },
            }
        }
        Ok(())
    }
    .await;
    let finish = view.finish().context("restoring the terminal");
    client.release_all().await;
    result.and(finish)
}

fn show_deck_error(state: &mut AgentDeckState, error: anyhow::Error) {
    let _ = state.step(
        AgentAction::EffectFailed {
            action_request_id: "standalone-operation".to_string(),
            message: format!("{error:#}"),
        },
        false,
    );
}

type DeckJob = futures::future::LocalBoxFuture<'static, Result<AgentResult>>;

async fn next_deck_job(job: &mut Option<DeckJob>) -> Result<AgentResult> {
    match job {
        Some(job) => job.await,
        None => std::future::pending().await,
    }
}

async fn next_deck_input(
    events: &mut Option<EventStream>,
) -> Option<std::io::Result<crossterm::event::Event>> {
    match events {
        Some(events) => events.next().await,
        None => std::future::pending().await,
    }
}

async fn export_history(run_id: &str, content: &str) -> Result<PathBuf> {
    let directory = std::env::current_dir().context("resolving the export directory")?;
    let stem = sanitize_file_component(run_id);
    for suffix in 0..1_000_u16 {
        let name = if suffix == 0 {
            format!("bitrouter-agent-{stem}.md")
        } else {
            format!("bitrouter-agent-{stem}-{suffix}.md")
        };
        let path = directory.join(name);
        match tokio::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(&path)
            .await
        {
            Ok(mut file) => {
                use tokio::io::AsyncWriteExt as _;
                file.write_all(content.as_bytes()).await?;
                file.flush().await?;
                return Ok(path);
            }
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(error).context("creating the agent history export"),
        }
    }
    bail!("could not choose an unused agent history export name")
}

fn sanitize_file_component(value: &str) -> String {
    let value = value
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '-' | '_') {
                character
            } else {
                '-'
            }
        })
        .collect::<String>();
    if value.is_empty() {
        "run".to_string()
    } else {
        value
    }
}

fn ensure_interactive(stdin: bool, stdout: bool) -> Result<()> {
    if stdin && stdout {
        Ok(())
    } else {
        bail!("bro agents requires an interactive stdin and stdout; use sessions --json")
    }
}

/// Apply one presentation-neutral result to the standalone reducer.
pub fn apply_result(state: &mut AgentDeckState, result: AgentResult) -> Vec<AgentEffect> {
    match result {
        AgentResult::Snapshot(snapshot) => {
            if snapshot.sequence >= state.snapshot().sequence {
                let _ = state.replace_snapshot(snapshot);
            }
            Vec::new()
        }
        AgentResult::History(history) => {
            state.replace_history(*history);
            Vec::new()
        }
        AgentResult::HistoryEvent { run_id, event } => {
            if state.selected_run_id() == Some(run_id.as_str()) {
                state.apply_history_event(event)
            } else {
                Vec::new()
            }
        }
        AgentResult::Action(action) => state.step(action, false),
        AgentResult::Diagnostics(diagnostics) => diagnostics
            .into_iter()
            .flat_map(|message| {
                state.step(
                    AgentAction::EffectFailed {
                        action_request_id: "diagnostic".to_string(),
                        message,
                    },
                    false,
                )
            })
            .collect(),
        AgentResult::Batch(results) => results
            .into_iter()
            .flat_map(|result| apply_result(state, result))
            .collect(),
        AgentResult::Exit => Vec::new(),
    }
}

/// Apply one result to Code without holding its state across daemon I/O. The
/// repaint bit observes collapsed-strip semantics while still repainting
/// expanded list, inspector, and acknowledgement changes.
pub fn apply_code_result(state: &mut CodeState, result: AgentResult) -> (Vec<CodeEffect>, bool) {
    match result {
        AgentResult::Snapshot(snapshot) => {
            if snapshot.sequence < state.agents().snapshot().sequence {
                return (Vec::new(), false);
            }
            let expanded_change =
                !state.agents().is_collapsed() && state.agents().snapshot() != &snapshot;
            let collapsed_change = state.replace_agent_snapshot(snapshot);
            (Vec::new(), expanded_change || collapsed_change)
        }
        AgentResult::History(history) => {
            if state.foreground_permission_pending() && !state.agents().is_inspector() {
                let effects = state.decline_agent_history(&history);
                return (effects, false);
            }
            let repaint = state.replace_agent_history(*history);
            (Vec::new(), repaint)
        }
        AgentResult::HistoryEvent { run_id, event } => {
            if state.agents().selected_run_id() == Some(run_id.as_str()) {
                let effects = state.apply_agent_history_event(event);
                (effects, state.agents().is_inspector())
            } else {
                (Vec::new(), false)
            }
        }
        AgentResult::Action(action) => (state.step_agent(action), true),
        AgentResult::Diagnostics(diagnostics) => {
            let mut effects = Vec::new();
            for message in diagnostics {
                effects.extend(state.step_agent(AgentAction::EffectFailed {
                    action_request_id: "diagnostic".to_string(),
                    message,
                }));
            }
            (effects, true)
        }
        AgentResult::Batch(results) => {
            let mut effects = Vec::new();
            let mut repaint = false;
            for result in results {
                let (next, changed) = apply_code_result(state, result);
                effects.extend(next);
                repaint |= changed;
            }
            (effects, repaint)
        }
        AgentResult::Exit => (Vec::new(), false),
    }
}

fn result_requests_exit(result: &AgentResult) -> bool {
    match result {
        AgentResult::Exit => true,
        AgentResult::Batch(results) => results.iter().any(result_requests_exit),
        _ => false,
    }
}

fn map_deck_snapshot(
    runs: Vec<RunSnapshot>,
    client_id: &str,
    new_run_target: Option<NewAgentRunTarget>,
    sequence: u64,
) -> AgentDeckSnapshot {
    AgentDeckSnapshot {
        sequence,
        runs: runs.iter().map(|run| map_run(run, client_id)).collect(),
        new_run_target,
    }
}

fn permission_context_detail(
    tool_call: &agent_client_protocol::schema::v1::ToolCallUpdate,
) -> String {
    let fields = &tool_call.fields;
    let has_detail = tool_call.meta.is_some()
        || fields.kind.is_some()
        || fields.status.is_some()
        || fields.content.is_some()
        || fields.locations.is_some()
        || fields.raw_input.is_some()
        || fields.raw_output.is_some();
    if !has_detail {
        return String::new();
    }
    serialized_update_pretty(tool_call)
}

fn map_run(run: &RunSnapshot, client_id: &str) -> AgentRunView {
    let attention = match run.attention {
        AttentionState::None => AgentAttention::None,
        AttentionState::Question => AgentAttention::Question {
            question_id: format!("question:{}:{}", run.run_id, run.last_seq),
            title: "Agent needs an answer".to_string(),
            detail: run.activity.clone(),
        },
        AttentionState::Permission => match run.pending_permissions.first() {
            Some(permission) => {
                let title = match permission.tool_call.fields.title.clone() {
                    Some(title) => title,
                    None => permission.tool_call.tool_call_id.0.to_string(),
                };
                let detail = permission_context_detail(&permission.tool_call);
                AgentAttention::Permission(AgentPermissionView {
                    permission_id: permission.permission_id.clone(),
                    requires_inspector: !detail.is_empty()
                        || title.chars().count() > 160
                        || permission.options.len() > 4,
                    title,
                    detail,
                    options: permission
                        .options
                        .iter()
                        .map(|option| AgentPermissionOption {
                            id: option.option_id.0.to_string(),
                            label: option.name.clone(),
                        })
                        .collect(),
                })
            }
            None => AgentAttention::Error {
                message: "Permission input is unavailable; refresh the run".to_string(),
            },
        },
        AttentionState::Result => AgentAttention::Result {
            summary: run.activity.clone(),
        },
        AttentionState::Error => AgentAttention::Error {
            message: match run.failure.clone() {
                Some(failure) => failure,
                None => run.activity.clone(),
            },
        },
    };
    AgentRunView {
        run_id: run.run_id.clone(),
        native_session_id: run.native_session_id.clone(),
        label: run.label.clone(),
        agent: run.agent_id.clone(),
        directory: run.cwd.display().to_string(),
        parent_run_id: run.parent_run_id.clone(),
        process: match run.process {
            ProcessState::Starting => AgentProcessState::Starting,
            ProcessState::Running => AgentProcessState::Running,
            ProcessState::Stopping => AgentProcessState::Stopping,
            ProcessState::Stopped => AgentProcessState::Stopped,
            ProcessState::Failed => AgentProcessState::Failed,
            ProcessState::Interrupted => AgentProcessState::Interrupted,
        },
        turn: match run.turn {
            TurnState::Idle => AgentTurnState::Idle,
            TurnState::Submitting => AgentTurnState::Submitting,
            TurnState::Working => AgentTurnState::Working,
            TurnState::Cancelling => AgentTurnState::Cancelling,
        },
        attention,
        review: match run.review {
            ReviewState::Unread => AgentReviewState::Unread,
            ReviewState::Reviewed => AgentReviewState::Reviewed,
        },
        activity: run.activity.clone(),
        confirmed_route: run.confirmed_route.clone(),
        attributed_cost: run
            .attributed_cost
            .as_ref()
            .map(|cost| format!("{} {:.6} ({})", cost.currency, cost.amount, cost.source)),
        failure: run.failure.clone(),
        lease: match run.lease.as_ref() {
            Some(lease) => AgentLeaseView {
                generation: lease.generation,
                owner: Some(lease.owner_client_id.clone()),
                owned_by_client: lease.owner_client_id == client_id,
            },
            None => AgentLeaseView::default(),
        },
        pinned: false,
        age_label: None,
        last_seq: run.last_seq,
    }
}

fn map_history_snapshot(attachment: RunAttachment, client_id: &str) -> AgentHistorySnapshot {
    AgentHistorySnapshot {
        run: map_run(&attachment.snapshot, client_id),
        first_retained_seq: attachment.replay.first_retained_seq,
        snapshot_seq: attachment.replay.snapshot_seq,
        history_complete: attachment.replay.history_complete,
        events: attachment
            .replay
            .events
            .iter()
            .map(map_history_event)
            .collect(),
    }
}

fn map_history_event(event: &SessionEvent) -> AgentHistoryEvent {
    let (kind, text) = match &event.kind {
        SessionEventKind::Lifecycle { process } => (
            AgentHistoryKind::Status,
            format!("Process state: {}", process_label(*process)),
        ),
        SessionEventKind::Turn { turn } => (
            AgentHistoryKind::Status,
            format!("Turn state: {}", turn_label(*turn)),
        ),
        SessionEventKind::Update { update } => map_update(update),
        SessionEventKind::PayloadOmitted { description } => (
            AgentHistoryKind::Status,
            format!("Payload omitted: {description}"),
        ),
        SessionEventKind::Permission { permission } => {
            let title = match permission.tool_call.fields.title.clone() {
                Some(title) => title,
                None => permission.tool_call.tool_call_id.0.to_string(),
            };
            let detail = permission_context_detail(&permission.tool_call);
            let options = permission
                .options
                .iter()
                .map(|option| option.name.as_str())
                .collect::<Vec<_>>()
                .join(", ");
            let context = if detail.is_empty() {
                String::new()
            } else {
                format!("\nContext:\n{detail}")
            };
            (
                AgentHistoryKind::Permission,
                format!("{title}{context}\nOptions: {options}"),
            )
        }
        SessionEventKind::PermissionResolved {
            permission_id,
            option_id,
            automatic,
        } => (
            AgentHistoryKind::Permission,
            format!(
                "Permission {permission_id} resolved as {}{}",
                option_id.as_deref().unwrap_or("cancelled"),
                if *automatic { " automatically" } else { "" }
            ),
        ),
        SessionEventKind::Activity { text } => (AgentHistoryKind::Status, text.clone()),
        SessionEventKind::UserPrompt { text } => (AgentHistoryKind::User, text.clone()),
        SessionEventKind::TurnSettled { result } => {
            let payload = match &result.result {
                Some(value) => format!("\n{value}"),
                None => String::new(),
            };
            (
                AgentHistoryKind::Result,
                format!("{}{}", result.stop_reason, payload),
            )
        }
        SessionEventKind::TurnFailed { message } => (AgentHistoryKind::Error, message.clone()),
        SessionEventKind::NativeSessionSelected {
            native_session_id,
            agent_session_id,
            initial_settings: _,
        } => (
            AgentHistoryKind::Status,
            format!(
                "Native session selected: {native_session_id}{}",
                match agent_session_id {
                    Some(id) => format!(" ({id})"),
                    None => String::new(),
                }
            ),
        ),
        SessionEventKind::Lease { lease } => (
            AgentHistoryKind::Status,
            match lease.as_ref() {
                Some(lease) => format!("Control lease held by {}", lease.owner_client_id),
                None => "Control lease released".to_string(),
            },
        ),
        SessionEventKind::Review { review } => (
            AgentHistoryKind::Status,
            format!(
                "Review state: {}",
                match review {
                    ReviewState::Unread => "unread",
                    ReviewState::Reviewed => "reviewed",
                }
            ),
        ),
    };
    AgentHistoryEvent {
        seq: event.seq,
        kind,
        text,
    }
}

fn map_update(
    update: &agent_client_protocol::schema::v1::SessionUpdate,
) -> (AgentHistoryKind, String) {
    match update {
        agent_client_protocol::schema::v1::SessionUpdate::ToolCall(tool_call) => {
            return (
                AgentHistoryKind::Tool,
                format!(
                    "{} · {:?}\n{}",
                    tool_call.title,
                    tool_call.status,
                    serialized_update_pretty(update)
                ),
            );
        }
        agent_client_protocol::schema::v1::SessionUpdate::ToolCallUpdate(tool_call) => {
            let title = tool_call
                .fields
                .title
                .clone()
                .unwrap_or_else(|| tool_call.tool_call_id.0.to_string());
            let status = tool_call
                .fields
                .status
                .as_ref()
                .map(|status| format!(" · {status:?}"))
                .unwrap_or_default();
            return (
                AgentHistoryKind::Tool,
                format!("{title}{status}\n{}", serialized_update_pretty(update)),
            );
        }
        _ => {}
    }
    match bitrouter_sdk::acp::translate::translate(update.clone()) {
        Some(SessionUpdateKind::MessageChunk { text, .. }) => (AgentHistoryKind::Assistant, text),
        Some(SessionUpdateKind::ThoughtChunk { text, .. }) => (AgentHistoryKind::Thought, text),
        Some(SessionUpdateKind::ToolCall {
            title,
            status,
            diff,
            ..
        }) => (
            AgentHistoryKind::Tool,
            format!(
                "{title} · {status:?}{}",
                match diff {
                    Some(diff) => format!("\n{diff}"),
                    None => String::new(),
                }
            ),
        ),
        Some(SessionUpdateKind::ToolCallUpdate {
            id,
            status,
            title,
            diff,
        }) => (
            AgentHistoryKind::Tool,
            format!(
                "{} · {}{}",
                match title {
                    Some(title) => title,
                    None => id,
                },
                match status {
                    Some(status) => format!("{status:?}"),
                    None => "updated".to_string(),
                },
                match diff {
                    Some(diff) => format!("\n{diff}"),
                    None => String::new(),
                }
            ),
        ),
        Some(update) => (AgentHistoryKind::Status, serialized_update(&update)),
        None => (AgentHistoryKind::Status, serialized_update(update)),
    }
}

fn serialized_update(update: &impl serde::Serialize) -> String {
    match serde_json::to_string(update) {
        Ok(text) => text,
        Err(error) => format!("ACP update could not be displayed: {error}"),
    }
}

fn serialized_update_pretty(update: &impl serde::Serialize) -> String {
    match serde_json::to_string_pretty(update) {
        Ok(text) => text,
        Err(error) => format!("ACP update could not be displayed: {error}"),
    }
}

fn process_label(process: ProcessState) -> &'static str {
    match process {
        ProcessState::Starting => "starting",
        ProcessState::Running => "running",
        ProcessState::Stopping => "stopping",
        ProcessState::Stopped => "stopped",
        ProcessState::Failed => "failed",
        ProcessState::Interrupted => "interrupted",
    }
}

fn turn_label(turn: TurnState) -> &'static str {
    match turn {
        TurnState::Idle => "idle",
        TurnState::Submitting => "submitting",
        TurnState::Working => "working",
        TurnState::Cancelling => "cancelling",
    }
}

fn accepted(action_request_id: String) -> AgentResult {
    AgentResult::Action(AgentAction::MutationAccepted { action_request_id })
}

fn diagnostics_result(diagnostics: Vec<String>) -> AgentResult {
    AgentResult::Diagnostics(diagnostics)
}

fn effect_action_request_id(effect: &AgentEffect) -> Option<String> {
    effect.action_request_id().map(ToString::to_string)
}

fn effect_transient_run_id(effect: &AgentEffect) -> Option<String> {
    match effect {
        AgentEffect::ReleaseLease { run_id, .. }
        | AgentEffect::Reply { run_id, .. }
        | AgentEffect::RespondPermission { run_id, .. }
        | AgentEffect::CancelTurn { run_id, .. }
        | AgentEffect::MarkReviewed { run_id, .. }
        | AgentEffect::Stop { run_id, .. } => Some(run_id.clone()),
        _ => None,
    }
}

fn action_request_id() -> String {
    uuid::Uuid::new_v4().to_string()
}

fn unexpected<T>(operation: &str, _: SessionResponse) -> Result<T> {
    bail!("daemon returned an unexpected response to {operation}")
}

#[cfg(test)]
mod tests {
    use agent_client_protocol::schema::v1::{
        ContentBlock, SessionUpdate, TextContent, ToolCallContent, ToolCallUpdate,
        ToolCallUpdateFields,
    };
    use anyhow::Context as _;
    use bitrouter_tui::agents::{
        AgentAction, AgentDeckMode, AgentDeckSnapshot, AgentDeckState, AgentEffect,
        AgentHistorySnapshot, AgentLeaseView, AgentRunView,
    };
    use bitrouter_tui::code::{CodeAction, CodeEffect, CodeState};
    use bitrouter_tui::permission::Prompt;
    use crossterm::event::{Event, KeyCode, KeyEvent, KeyModifiers};

    use crate::supervisor::SessionScope;

    use super::{
        AgentResult, BackgroundClient, apply_code_result, apply_result, effect_action_request_id,
        map_update, permission_context_detail,
    };

    #[test]
    fn permission_detail_preserves_context_without_terminal_controls() {
        let fields = ToolCallUpdateFields::default()
            .title("Run command".to_string())
            .raw_input(serde_json::json!({"command": "\u{1b}[31mcargo test"}));
        let detail = permission_context_detail(&ToolCallUpdate::new("tool-1", fields));
        assert!(detail.contains("rawInput"));
        assert!(detail.contains("cargo test"));
        assert!(!detail.contains('\u{1b}'));

        let title_only = ToolCallUpdateFields::default().title("Read file".to_string());
        assert!(permission_context_detail(&ToolCallUpdate::new("tool-2", title_only)).is_empty());
    }

    #[test]
    fn retained_tool_update_keeps_raw_output_and_text_content() {
        let fields = ToolCallUpdateFields::default()
            .title("Inspect complete output".to_string())
            .content(vec![ToolCallContent::from(ContentBlock::Text(
                TextContent::new("full tool text that must remain reviewable"),
            ))])
            .raw_output(serde_json::json!({
                "stdout": "exact raw output",
                "exitCode": 0,
            }));
        let (_, text) = map_update(&SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            "tool-retained",
            fields,
        )));
        assert!(text.contains("full tool text that must remain reviewable"));
        assert!(text.contains("exact raw output"));
        assert!(text.contains("exitCode"));
    }

    #[test]
    fn foreground_permission_declines_in_flight_attach_history() -> anyhow::Result<()> {
        let mut state = CodeState::default();
        state.set_agent_client_id("test-client");
        let mut run = AgentRunView::new("run-1", "worker", "stub", "/tmp/worktree");
        run.lease = AgentLeaseView {
            generation: 7,
            owner: Some("test-client".to_string()),
            owned_by_client: true,
        };
        let _ = state.replace_agent_snapshot(AgentDeckSnapshot {
            sequence: 1,
            runs: vec![run.clone()],
            new_run_target: None,
        });
        let _ = state.step(CodeAction::Event(Event::Key(KeyEvent::new(
            KeyCode::Char('/'),
            KeyModifiers::NONE,
        ))));
        for character in "background agents".chars() {
            let _ = state.step(CodeAction::Event(Event::Key(KeyEvent::new(
                KeyCode::Char(character),
                KeyModifiers::NONE,
            ))));
        }
        let _ = state.step(CodeAction::Event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))));
        let attach = state.step_agent(AgentAction::Event(Event::Key(KeyEvent::new(
            KeyCode::Enter,
            KeyModifiers::NONE,
        ))));
        let attach_request_id = attach
            .iter()
            .find_map(|effect| match effect {
                CodeEffect::Agent(AgentEffect::Attach {
                    run_id,
                    action_request_id,
                }) if run_id == "run-1" => Some(action_request_id.clone()),
                _ => None,
            })
            .context("owned run did not begin an attach")?;

        assert!(
            state
                .receive_permission(Prompt::new(
                    "foreground-permission",
                    Some("Foreground needs approval".to_string()),
                    "foreground-tool",
                    None,
                    Vec::new(),
                ))
                .is_empty()
        );
        let history = AgentHistorySnapshot {
            run,
            first_retained_seq: 1,
            snapshot_seq: 1,
            history_complete: true,
            events: Vec::new(),
        };
        let (effects, repaint) = apply_code_result(
            &mut state,
            AgentResult::Batch(vec![
                AgentResult::History(Box::new(history)),
                AgentResult::Action(AgentAction::MutationAccepted {
                    action_request_id: attach_request_id.clone(),
                }),
            ]),
        );

        assert!(repaint);
        assert!(!state.agents().is_inspector());
        let detach = effects.iter().find_map(|effect| match effect {
            CodeEffect::Agent(AgentEffect::Detach {
                run_id,
                lease_generation,
                action_request_id,
            }) => Some((
                run_id.as_str(),
                *lease_generation,
                action_request_id.as_str(),
            )),
            _ => None,
        });
        let (run_id, generation, detach_request_id) =
            detach.context("declined attach did not emit a fenced detach")?;
        assert_eq!(run_id, "run-1");
        assert_eq!(generation, 7);
        assert_ne!(detach_request_id, attach_request_id);
        Ok(())
    }

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
        assert_eq!(
            BackgroundClient::attach_interactive("bitrouter.sock".into()).scopes,
            [
                SessionScope::Peek,
                SessionScope::Transcript,
                SessionScope::Attach,
                SessionScope::Respond,
                SessionScope::Stop,
            ]
            .into_iter()
            .collect()
        );
    }

    #[test]
    fn older_in_flight_snapshot_cannot_replace_newer_state() {
        let mut state = AgentDeckState::new(AgentDeckMode::Standalone, "client");
        let _ = apply_result(
            &mut state,
            AgentResult::Snapshot(AgentDeckSnapshot {
                sequence: 2,
                ..AgentDeckSnapshot::default()
            }),
        );
        let _ = apply_result(
            &mut state,
            AgentResult::Snapshot(AgentDeckSnapshot {
                sequence: 1,
                ..AgentDeckSnapshot::default()
            }),
        );
        assert_eq!(state.snapshot().sequence, 2);
    }

    #[test]
    fn confirmed_takeover_keeps_its_id_for_failure_recovery() {
        let effect = AgentEffect::Takeover {
            run_id: "run-1".to_string(),
            action_request_id: "takeover-1".to_string(),
        };
        assert_eq!(
            effect_action_request_id(&effect).as_deref(),
            Some("takeover-1")
        );
    }

    #[test]
    fn standalone_rejects_either_noninteractive_stream() {
        for streams in [(false, true), (true, false), (false, false)] {
            let result = super::ensure_interactive(streams.0, streams.1);
            assert!(result.is_err());
            if let Err(error) = result {
                assert!(error.to_string().contains("sessions --json"));
            }
        }
        assert!(super::ensure_interactive(true, true).is_ok());
    }
}
