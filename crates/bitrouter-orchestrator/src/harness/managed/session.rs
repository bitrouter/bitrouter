//! One CoreSession drives the native harness. Workspace tools and approvals
//! remain local, with their start ledger serialized against checkpoint fences.

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bitrouter_sdk::{App, caller::CallerContext};
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use super::store::{Journal, MAX_STORE_BYTES, NativePort, NativeStore, StartedTool, error};
use super::{super::HarnessConfig, NativeResources};
use crate::agent::ToolMode;
use crate::core::checkpoint::{ToolStartFence, sha256};
use crate::core::protocol::{
    Bind, Capabilities, CoreError, ErrorCode, OwnershipGrant, Restore, ServerMessage, TaskInput,
    ToolEffect, ToolExecute, ToolObservation, ToolOutcome, ToolResult, ToolStatus, VERSION,
};
use crate::core::session::{CoreSession, HarnessPort, RunStatus, SessionSnapshot};
use crate::service::workspace::WorkspaceFence;
use crate::store::ExecutionOwner;

#[async_trait]
pub trait NativeApproval: Send + Sync {
    async fn approve(&self, command: &ToolExecute) -> Result<bool, CoreError>;
}

pub struct NativeSession {
    core: CoreSession,
    port: Arc<NativePort>,
    resources: Arc<NativeResources>,
    receiver: mpsc::Receiver<ServerMessage>,
    workspace: Arc<WorkspaceFence>,
    effects: Arc<RwLock<()>>,
}

impl NativeSession {
    /// Resume only a durably released session. Abruptly lost native effects or
    /// activity require investigation; a new database revision is not proof.
    pub async fn open(
        app: Arc<App>,
        caller: CallerContext,
        store: Arc<dyn NativeStore>,
        workspace: &Path,
        mode: ToolMode,
        config: &HarnessConfig,
    ) -> Result<Self, CoreError> {
        let workspace = workspace
            .canonicalize()
            .map_err(|err| error(err.to_string()))?;
        let loaded = store.load().await?;
        let (revision, previous) = match loaded {
            Some((revision, bytes)) => {
                if bytes.len() > MAX_STORE_BYTES {
                    return Err(error("native session exceeds storage bound"));
                }
                let journal: Journal =
                    serde_json::from_slice(&bytes).map_err(|_| error("invalid native journal"))?;
                if journal.format != 1
                    || !journal.released
                    || journal.workspace != workspace
                    || journal.mode != mode
                {
                    return Err(CoreError::rejected(
                        ErrorCode::RecoveryRequired,
                        "native session requires a matching workspace/mode and a confirmed release",
                    ));
                }
                (revision, Some(journal))
            }
            None => (0, None),
        };
        let epoch = previous.as_ref().map_or(Ok(1), |journal| {
            journal
                .grant
                .execution_epoch
                .checked_add(1)
                .ok_or_else(|| error("native epoch exhausted"))
        })?;
        let core_id = format!("core_{}", uuid::Uuid::new_v4());
        let grant = OwnershipGrant {
            session_id: previous.as_ref().map_or_else(
                || format!("session_{}", uuid::Uuid::new_v4()),
                |journal| journal.grant.session_id.clone(),
            ),
            harness_id: previous.as_ref().map_or_else(
                || format!("harness_{}", uuid::Uuid::new_v4()),
                |journal| journal.grant.harness_id.clone(),
            ),
            core_instance_id: core_id.clone(),
            execution_epoch: epoch,
        };
        let owner = ExecutionOwner {
            server_instance_id: core_id,
            generation: epoch,
            stopped_at_ms: None,
        };
        let fence = Arc::new(
            WorkspaceFence::acquire(&workspace, &owner, &grant.session_id)
                .map_err(|err| error(err.to_string()))?,
        );
        let resources =
            match NativeResources::discover(&workspace, mode, config, &CancellationToken::new())
                .await
            {
                Ok(resources) => Arc::new(resources),
                Err(err) => {
                    if !err.cleanup_unknown {
                        fence.finish().map_err(|err| error(err.to_string()))?;
                    }
                    return Err(error(err.to_string()));
                }
            };
        let initialized = async {
            let mut journal = previous.clone().unwrap_or_else(|| {
                Journal::new(workspace, mode, grant.clone(), resources.manifest().clone())
            });
            journal.grant = grant.clone();
            journal.manifest = resources.manifest().clone();
            journal.released = false;
            let (sender, receiver) = mpsc::channel(32);
            let port = Arc::new(NativePort::new(revision, journal.clone(), store, sender));
            {
                let mut state = port.state.lock().await;
                port.persist(&mut state, journal.clone()).await?;
            }
            let caps = Capabilities {
                version: VERSION,
                core_instance_id: grant.core_instance_id.clone(),
                operations: vec![],
                transports: vec!["in_process".into()],
                unsupported_features: vec!["running_restore_handoff".into()],
                limits: journal.limits.clone(),
                max_sessions: 1,
                max_host_model_attempts: journal.limits.active_models,
            };
            let binding = Bind {
                grant: grant.clone(),
                durable_head: journal.head.clone(),
                checkpoint: journal.checkpoint.clone(),
                manifest: journal.manifest.clone(),
                limits: journal.limits.clone(),
            };
            let core = if previous.is_some() {
                CoreSession::restore(
                    Restore {
                        binding,
                        journal_tail: vec![],
                        tools: vec![],
                        results: vec![],
                        available_artifacts: journal.available(),
                        previous_owner_stopped: true,
                        active_time: None,
                    },
                    &caps,
                    app,
                    caller,
                    port.clone(),
                )
                .await?
            } else {
                CoreSession::bind(binding, &caps, app, caller, port.clone()).await?
            };
            let revision = core
                .snapshot()
                .await
                .signals
                .revision
                .checked_add(1)
                .ok_or_else(|| error("native signal revision exhausted"))?;
            core.signals(
                &operation("signals", &epoch.to_string()),
                resources.signals(&grant, revision),
            )
            .await?;
            Ok(Self {
                core,
                port,
                resources: resources.clone(),
                receiver,
                workspace: fence.clone(),
                effects: Arc::new(RwLock::new(())),
            })
        }
        .await;
        if initialized.is_err() {
            // No core driver or workspace tool has started during initialization.
            // A failed journal remains blocked, but confirmed resource shutdown
            // permits a different session to use this workspace.
            resources.shutdown().await.map_err(error)?;
            fence.finish().map_err(|err| error(err.to_string()))?;
        }
        initialized
    }

    pub fn core(&self) -> &CoreSession {
        &self.core
    }

    pub async fn run(
        &mut self,
        input: TaskInput,
        approval: Arc<dyn NativeApproval>,
        cancel: CancellationToken,
    ) -> Result<SessionSnapshot, CoreError> {
        self.core
            .start(
                &format!("input_{}", uuid::Uuid::new_v4()),
                self.core.head().await.state_revision,
                input,
            )
            .await?;
        let result = self.drive(approval, cancel).await;
        if result.is_err() {
            self.port.cancel_tools();
            self.core.disconnect().await;
        }
        result
    }

    async fn drive(
        &mut self,
        approval: Arc<dyn NativeApproval>,
        cancel: CancellationToken,
    ) -> Result<SessionSnapshot, CoreError> {
        let core = self.core.clone();
        let mut driver = drive_task(&core);
        let mut driving = true;
        let mut needs_drive = false;
        let mut cancelling = false;
        let mut jobs = tokio::task::JoinSet::new();
        let outcome = loop {
            tokio::select! {
                result = &mut driver, if driving => {
                    driving = false;
                    match result.map_err(|err| error(err.to_string())).and_then(|result| result) {
                        Ok(snapshot) if terminal(&snapshot) => break Ok(snapshot),
                        Ok(snapshot) => {
                            if needs_drive {
                                driver = drive_task(&core);
                                driving = true;
                                needs_drive = false;
                            } else if jobs.is_empty() && self.receiver.is_empty() {
                                break Ok(snapshot);
                            }
                        }
                        Err(err) => break Err(err),
                    }
                }
                _ = cancel.cancelled(), if !cancelling => {
                    cancelling = true;
                    if let Err(err) = cancel_current_run(&core).await {
                        break Err(err);
                    }
                    self.port.cancel_tools();
                    if driving { needs_drive = true; } else { driver = drive_task(&core); driving = true; needs_drive = false; }
                }
                command = self.receiver.recv() => {
                    match command {
                        Some(ServerMessage::ToolExecute(command)) => {
                            let (port, resources, fence, effects, approval, core) = (self.port.clone(), self.resources.clone(), self.workspace.clone(), self.effects.clone(), approval.clone(), core.clone());
                            jobs.spawn(async move { execute(&core, port, resources, fence, effects, approval, *command).await });
                        }
                        Some(ServerMessage::MaterialRequest { request_id, material_id, version }) => {
                            let (material, unavailable) = match self.resources.material(&material_id, &version).await { Ok(material) => (Some(material), None), Err(reason) => (None, Some(reason)) };
                            if let Err(err) = core.material_result(&operation("material", &request_id), &request_id, material, unavailable).await {
                                break Err(err);
                            }
                            if driving { needs_drive = true; } else { driver = drive_task(&core); driving = true; needs_drive = false; }
                        }
                        _ => break Err(error("native command channel closed")),
                    }
                }
                result = jobs.join_next(), if !jobs.is_empty() => {
                    match result {
                        Some(Ok(Ok(()))) => { if driving { needs_drive = true; } else { driver = drive_task(&core); driving = true; needs_drive = false; } }
                        Some(Ok(Err(err))) => break Err(err),
                        _ => break Err(error("native tool worker was lost; effects require reconciliation")),
                    }
                }
            }
        };
        if outcome.is_err() {
            core.disconnect().await;
            self.port.cancel_tools();
        }
        // Keep polling the core so SDK work settles; dropping the driver does not
        // prove it stopped. Native shell workers similarly must be joined.
        if driving {
            let _ = driver.await;
        }
        let mut outcome = outcome;
        while let Some(result) = jobs.join_next().await {
            let settled = result
                .map_err(|err| error(err.to_string()))
                .and_then(|result| result);
            if let Err(err) = settled {
                outcome = Err(err);
            }
        }
        outcome
    }

    pub async fn close(self) -> Result<(), CoreError> {
        self.resources.shutdown().await.map_err(error)?;
        self.core
            .release(
                &format!("release_{}", uuid::Uuid::new_v4()),
                self.core.head().await.state_revision,
            )
            .await?;
        let mut state = self.port.state.lock().await;
        let mut next = state.journal.clone();
        next.released = true;
        self.port.persist(&mut state, next).await?;
        self.workspace
            .finish()
            .map_err(|err| error(err.to_string()))
    }
}

fn drive_task(core: &CoreSession) -> tokio::task::JoinHandle<Result<SessionSnapshot, CoreError>> {
    // Controls and materials may await a commit held by this driver. Give it
    // independent polling so a database await cannot deadlock the native actor.
    let core = core.clone();
    tokio::spawn(async move { core.drive().await })
}

async fn cancel_current_run(core: &CoreSession) -> Result<(), CoreError> {
    let Some(run) = core.snapshot().await.run else {
        return Ok(());
    };
    loop {
        let snapshot = core.snapshot().await;
        if terminal(&snapshot)
            || snapshot
                .run
                .as_ref()
                .is_none_or(|current| current.run_id != run.run_id)
        {
            return Ok(());
        }
        let revision = core.head().await.state_revision;
        match core
            .cancel_run(
                &operation("cancel", &format!("{}:{revision}", run.run_id)),
                revision,
                &run.run_id,
            )
            .await
        {
            Ok(_) => return Ok(()),
            Err(err)
                if err.code == ErrorCode::StaleRevision
                    && err.commit_status == crate::core::protocol::CommitStatus::NotCommitted =>
            {
                continue;
            }
            Err(err) if err.code == ErrorCode::Busy && terminal(&core.snapshot().await) => {
                return Ok(());
            }
            Err(err) => return Err(err),
        }
    }
}

fn terminal(snapshot: &SessionSnapshot) -> bool {
    snapshot.run.as_ref().is_some_and(|run| {
        matches!(
            run.status,
            RunStatus::Completed
                | RunStatus::Failed
                | RunStatus::Cancelled
                | RunStatus::RecoveryRequired
        )
    })
}

fn operation(kind: &str, identity: &str) -> String {
    format!("{kind}_{}", sha256(identity.as_bytes()))
}

async fn execute(
    core: &CoreSession,
    port: Arc<NativePort>,
    resources: Arc<NativeResources>,
    fence: Arc<WorkspaceFence>,
    effects: Arc<RwLock<()>>,
    approval: Arc<dyn NativeApproval>,
    command: ToolExecute,
) -> Result<(), CoreError> {
    let cancel = CancellationToken::new();
    let declaration = resources
        .manifest()
        .tools
        .iter()
        .find(|tool| tool.name == command.tool)
        .ok_or_else(|| error("native tool missing from inventory"))?;
    {
        let mut state = port.state.lock().await;
        port.authorize_dispatch().await?;
        if command.execution_epoch != state.journal.grant.execution_epoch
            || command.authorizing_event_seq > state.journal.head.event_seq
        {
            return Err(error(
                "native tool has a stale or uncommitted authorization",
            ));
        }
        let payload = state
            .journal
            .checkpoint
            .as_ref()
            .ok_or_else(|| error("native tool has no checkpoint"))?
            .decode(&state.journal.limits)?;
        let snapshot: SessionSnapshot = serde_json::from_value(payload.checkpoint.state)
            .map_err(|_| error("native checkpoint snapshot invalid"))?;
        if !snapshot
            .agents
            .values()
            .filter_map(|agent| agent.turn.as_ref())
            .flat_map(|turn| &turn.invocations)
            .any(|call| call.dispatch == command && call.result.is_none())
        {
            return Err(error("native execute does not match a durable invocation"));
        }
        if let Some(previous) = state.journal.starts.get(&command.invocation_id) {
            if previous.command != command {
                return Err(error("duplicate native invocation changed"));
            }
            if let Some(result) = previous.result.clone() {
                drop(state);
                core.tool_result(&operation("result", &command.invocation_id), result)
                    .await?;
                return Ok(());
            }
            return Err(error(
                "native invocation already admitted; effects cannot restart",
            ));
        }
        let mut next = state.journal.clone();
        next.starts.insert(
            command.invocation_id.clone(),
            StartedTool {
                command: command.clone(),
                started: false,
                result: None,
            },
        );
        port.persist(&mut state, next).await?;
        port.active
            .lock()
            .map_err(|_| error("native execution lock poisoned"))?
            .insert(command.invocation_id.clone(), cancel.clone());
    }
    let approved = if declaration.approval_required {
        core.tool_status(
            &operation("approval", &command.invocation_id),
            ToolObservation {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
                status: ToolStatus::WaitingApproval,
                evidence: vec![],
            },
        )
        .await?;
        tokio::select! { _ = cancel.cancelled() => false, approved = approval.approve(&command) => approved? }
    } else {
        true
    };
    // Read guards can overlap, while writes, shells and unknown MCP effects take
    // exclusive access. Keep the guard through persistence of the local result.
    let read = if declaration.effect == ToolEffect::Read {
        Some(effects.read().await)
    } else {
        None
    };
    let write = if declaration.effect != ToolEffect::Read {
        Some(effects.write().await)
    } else {
        None
    };
    if approved && !cancel.is_cancelled() {
        // Account for admitted native activity before the worker starts. This
        // must happen outside the port lock because status commits use that lock.
        core.tool_status(
            &operation("running", &command.invocation_id),
            ToolObservation {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
                status: ToolStatus::Running,
                evidence: vec![],
            },
        )
        .await?;
    }
    let worker = {
        let mut state = port.state.lock().await;
        let denied = !approved
            || cancel.is_cancelled()
            || state.journal.fences.contains(&ToolStartFence {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
            });
        if denied {
            None
        } else {
            fence.validate().map_err(|err| error(err.to_string()))?;
            let mut next = state.journal.clone();
            next.starts
                .get_mut(&command.invocation_id)
                .ok_or_else(|| error("native start record missing"))?
                .started = true;
            port.persist(&mut state, next).await?;
            let (resources, command, cancel) = (resources.clone(), command.clone(), cancel.clone());
            Some(tokio::spawn(async move {
                resources.execute(&command, &cancel).await
            }))
        }
    };
    let result = match worker {
        Some(worker) => match worker.await {
            Ok(Ok(result)) => result,
            _ => ToolResult {
                invocation_id: command.invocation_id.clone(),
                attempt_id: command.attempt_id.clone(),
                status: ToolOutcome::EffectUnknown,
                output: "Native worker outcome is unavailable; do not retry effects.".into(),
                evidence: vec![],
                workspace_revision: None,
            },
        },
        None => ToolResult {
            invocation_id: command.invocation_id.clone(),
            attempt_id: command.attempt_id.clone(),
            status: if !approved && !cancel.is_cancelled() {
                ToolOutcome::Denied
            } else {
                ToolOutcome::NotExecuted
            },
            output: "Native execution was not authorized.".into(),
            evidence: vec![],
            workspace_revision: None,
        },
    };
    {
        let mut state = port.state.lock().await;
        let mut next = state.journal.clone();
        next.starts
            .get_mut(&command.invocation_id)
            .ok_or_else(|| error("native result has no start record"))?
            .result = Some(result.clone());
        port.persist(&mut state, next).await?;
        port.active
            .lock()
            .map_err(|_| error("native execution lock poisoned"))?
            .remove(&command.invocation_id);
    }
    drop((read, write));
    core.tool_result(&operation("result", &command.invocation_id), result)
        .await?;
    Ok(())
}
