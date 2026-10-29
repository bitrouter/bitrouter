//! One CoreSession drives the native harness. Workspace tools and approvals
//! remain local, with their start ledger serialized against checkpoint fences.

pub mod assessment;
pub(crate) mod owned;

use crate::storage::checkpoint::CheckpointBatchExt;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use crate::gateway::assembly::App;
use crate::gateway::caller::CallerContext;
use async_trait::async_trait;
use tokio::sync::{RwLock, mpsc};
use tokio_util::sync::CancellationToken;

use super::store::local::{Journal, NativePort, StartedTool, error};
use super::store::{MAX_STORE_BYTES, NativeStore};
use super::{super::HarnessConfig, NativeResources};
use crate::core::session::{CoreSession, HarnessPort};
use crate::harness::service::workspace::WorkspaceFence;
use crate::storage::checkpoint::sha256;
use crate::storage::checkpoint::snapshot::{RunStatus, SessionSnapshot};
use bitrouter_ai::core::checkpoint::{DurableHead, ToolStartFence};
use bitrouter_ai::core::protocol::{
    Bind, Capabilities, CoreError, ErrorCode, OperationReceipt, OwnershipGrant, Restore,
    ServerMessage, TaskInput, ToolEffect, ToolExecute, ToolObservation, ToolOutcome, ToolResult,
    ToolStatus, VERSION,
};
use bitrouter_ai::harness::agent::ToolMode;
use bitrouter_ai::harness::runtime::ExecutionOwner;

#[async_trait]
pub trait NativeApproval: Send + Sync {
    async fn approve(&self, command: &ToolExecute) -> Result<bool, CoreError>;
    /// Generic command approval is never blanket consent for an MCP skill.
    async fn approve_skill(
        &self,
        _request: &bitrouter_ai::core::skills::SkillConsentRequest,
    ) -> Result<bool, CoreError> {
        Ok(false)
    }
}

pub struct NativeSession {
    core: CoreSession,
    port: Arc<NativePort>,
    resources: Arc<NativeResources>,
    receiver: mpsc::Receiver<ServerMessage>,
    workspace: Arc<WorkspaceFence>,
    effects: Arc<RwLock<()>>,
    owned_handle: Option<owned::Handle>,
}

/// A passive view of the original native journal. Reading it neither claims
/// ownership nor authorizes recovery, resources, model calls or tools.
pub struct StoredSession {
    pub workspace: PathBuf,
    pub mode: ToolMode,
    pub grant: OwnershipGrant,
    pub head: DurableHead,
    pub released: bool,
    pub snapshot: Option<SessionSnapshot>,
}

struct Preparation<'a> {
    opening: Option<&'a mut owned::Opening>,
    cancel: &'a CancellationToken,
}

impl NativeSession {
    pub async fn inspect(store: &dyn NativeStore) -> Result<Option<StoredSession>, CoreError> {
        let Some((_, bytes)) = store.load().await? else {
            return Ok(None);
        };
        if bytes.len() > MAX_STORE_BYTES {
            return Err(error("native session exceeds storage bound"));
        }
        let journal: Journal =
            serde_json::from_slice(&bytes).map_err(|_| error("invalid native journal"))?;
        if journal.format != 1 || !journal.workspace.is_absolute() {
            return Err(error("invalid native journal format/workspace"));
        }
        journal.grant.validate()?;
        journal.head.validate()?;
        let snapshot = match (&journal.checkpoint, &journal.ack) {
            (Some(batch), Some(retained)) => {
                let available = journal
                    .available()
                    .into_iter()
                    .map(|artifact| (artifact.artifact_id.clone(), artifact))
                    .collect();
                let (ack, payload) = batch.validate_append_with_payload(
                    &journal.grant,
                    &journal.head,
                    &journal.limits,
                    &available,
                    Some(retained),
                )?;
                if ack.head() != journal.head {
                    return Err(error("native checkpoint does not match its durable head"));
                }
                let snapshot: SessionSnapshot = serde_json::from_value(payload.checkpoint.state)
                    .map_err(|_| error("invalid native checkpoint snapshot"))?;
                if snapshot.session_id != journal.grant.session_id {
                    return Err(error("native checkpoint belongs to another session"));
                }
                Some(snapshot)
            }
            (None, None) if journal.head == DurableHead::default() && !journal.released => None,
            _ => return Err(error("native journal has an incomplete checkpoint/ACK")),
        };
        Ok(Some(StoredSession {
            workspace: journal.workspace,
            mode: journal.mode,
            grant: journal.grant,
            head: journal.head,
            released: journal.released,
            snapshot,
        }))
    }

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
        app.require_legacy_core()?;
        Self::open_cancellable(
            app,
            caller,
            store,
            workspace,
            mode,
            config,
            &CancellationToken::new(),
        )
        .await
    }

    /// The daemon can cancel resource preparation without dropping a live
    /// native driver or claiming that unknown effects have settled.
    pub async fn open_cancellable(
        app: Arc<App>,
        caller: CallerContext,
        store: Arc<dyn NativeStore>,
        workspace: &Path,
        mode: ToolMode,
        config: &HarnessConfig,
        cancel: &CancellationToken,
    ) -> Result<Self, CoreError> {
        app.require_legacy_core()?;
        Self::open_prepared(
            app,
            caller,
            store,
            workspace,
            mode,
            config,
            Preparation {
                opening: None,
                cancel,
            },
        )
        .await
    }

    /// Open the persistent in-process harness with checked tenancy credentials.
    /// The local name selects within the authenticated principal's namespace.
    /// Workspace and tool configuration remain trusted host inputs, not a remote API.
    ///
    /// # Errors
    /// Rejects unsupported authority, an occupied host slot, incompatible retained
    /// identity, or unconfirmed journal/workspace cleanup. It never falls back to Legacy.
    pub async fn open_authenticated(
        app: Arc<App>,
        headers: http::HeaderMap,
        name: &str,
        workspace: &Path,
        mode: ToolMode,
        config: &HarnessConfig,
    ) -> Result<Self, CoreError> {
        owned::open(app, headers, name, workspace, mode, config).await
    }

    async fn open_inner(
        app: Arc<App>,
        caller: CallerContext,
        store: Arc<dyn NativeStore>,
        workspace: &Path,
        mode: ToolMode,
        config: &HarnessConfig,
        opening: Option<&mut owned::Opening>,
    ) -> Result<Self, CoreError> {
        let cancel = opening
            .as_ref()
            .map_or_else(CancellationToken::new, |opening| {
                opening.cancellation().clone()
            });
        Self::open_prepared(
            app,
            caller,
            store,
            workspace,
            mode,
            config,
            Preparation {
                opening,
                cancel: &cancel,
            },
        )
        .await
    }

    async fn open_prepared(
        app: Arc<App>,
        caller: CallerContext,
        store: Arc<dyn NativeStore>,
        workspace: &Path,
        mode: ToolMode,
        config: &HarnessConfig,
        preparation: Preparation<'_>,
    ) -> Result<Self, CoreError> {
        let Preparation {
            mut opening,
            cancel,
        } = preparation;
        if let Some(opening) = &opening {
            opening.check()?;
        } else {
            app.require_legacy_core()?;
        }
        let legacy_workspace = if opening.is_none() {
            Some(
                workspace
                    .canonicalize()
                    .map_err(|err| error(err.to_string()))?,
            )
        } else {
            None
        };
        let loaded = store.load().await?;
        if let Some(opening) = &opening {
            opening.check()?;
        }
        let workspace = match legacy_workspace {
            Some(workspace) => workspace,
            None => workspace
                .canonicalize()
                .map_err(|err| error(err.to_string()))?,
        };
        let (mut revision, previous) = match loaded {
            Some((revision, bytes)) => {
                if bytes.len() > MAX_STORE_BYTES {
                    return Err(error("native session exceeds storage bound"));
                }
                let journal: Journal =
                    serde_json::from_slice(&bytes).map_err(|_| error("invalid native journal"))?;
                if journal.format != 1
                    || (!journal.released
                        && opening
                            .as_ref()
                            .is_none_or(|opening| opening.store.recovery().is_none()))
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
        let mut grant = OwnershipGrant {
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
        if let Some(opening) = &opening {
            grant.harness_id = opening.store.descriptor().harness.clone();
            grant.session_id = opening.store.descriptor().session.clone();
        }
        if let Some(opening) = &mut opening {
            if opening.store.recovery().is_some() && !config.servers.is_empty() {
                return Err(CoreError::rejected(
                    ErrorCode::RecoveryRequired,
                    "stopped native recovery requires an empty MCP configuration",
                ));
            }
            let binding = opening
                .authority
                .bind(opening.store.descriptor().clone(), grant.clone())?;
            if let Some(checked) = opening.store.recovery() {
                binding.begin_recovery(checked.clone())?;
            }
            opening.store.attach(binding.clone())?;
            opening.binding = Some(binding);
        }
        let owner = ExecutionOwner {
            server_instance_id: core_id,
            generation: epoch,
            stopped_at_ms: None,
        };
        let fence = Arc::new(
            match opening
                .as_ref()
                .and_then(|opening| opening.store.recovery())
            {
                Some(checked) => WorkspaceFence::reserve_stopped_native(&workspace, checked),
                None => WorkspaceFence::acquire(&workspace, &owner, &grant.session_id),
            }
            .map_err(|err| error(err.to_string()))?,
        );
        if let Some(opening) = &mut opening {
            opening.workspace = Some(fence.clone());
        }
        if let Some(opening) = &mut opening
            && opening.store.recovery().is_none()
        {
            opening.epoch_started = true;
            let prior = previous
                .as_ref()
                .map(crate::managed_store::owned::encode)
                .transpose()
                .map_err(|_| error("native opening predecessor bound"))?;
            let next_revision = opening
                .store
                .begin_epoch(
                    revision,
                    &grant,
                    &workspace,
                    mode,
                    prior.as_deref(),
                    config.servers.is_empty(),
                )
                .await?;
            // Without a startup stamp, begin_epoch leaves the revision unchanged.
            // Only its acknowledged marker write advances the durable revision.
            opening.epoch_committed = next_revision != revision;
            revision = next_revision;
        }
        #[cfg(test)]
        if let Some(opening) = &opening {
            let gate = opening
                .authority
                .service()
                .take_host_gate(crate::tenancy::hosts::HostBoundary::Transport)?;
            if let Some(gate) = &gate {
                gate.mcp
                    .set(Arc::downgrade(&opening.mcp))
                    .map_err(|_| error("native test resource observer already set"))?;
            }
            opening.mcp.lock().await.set_discovery_gate(gate);
        }
        let discovered = match &opening {
            Some(opening) => {
                NativeResources::discover_in(
                    &workspace,
                    mode,
                    config,
                    &caller,
                    opening.cancellation(),
                    super::DiscoveryOwner {
                        mcp: opening.mcp.clone(),
                        discovery: &crate::gateway::mcp::client::Discovery::native(
                            opening.authority.clone(),
                            opening.cancellation().clone(),
                        ),
                    },
                    opening.authority.has_choices(),
                )
                .await
            }
            None => {
                NativeResources::discover_for_caller(&workspace, mode, config, &caller, cancel)
                    .await
            }
        };
        let resources = match discovered {
            Ok(resources) => Arc::new(resources),
            Err(err) => {
                if let Some(opening) = &mut opening {
                    opening.ambiguous = err.cleanup_unknown;
                }
                if !err.cleanup_unknown && opening.is_none() {
                    fence.finish().map_err(|err| error(err.to_string()))?;
                }
                return Err(error(err.to_string()));
            }
        };
        if let Some(opening) = &mut opening {
            opening.resources = Some(resources.clone());
            #[cfg(test)]
            opening.authority.service().host_resources_ready().await?;
            opening.check()?;
        }
        let initialized = async {
            let mut journal = previous.clone().unwrap_or_else(|| {
                Journal::new(workspace, mode, grant.clone(), resources.manifest().clone())
            });
            journal.grant = grant.clone();
            journal.manifest = resources.manifest().clone();
            journal.released = false;
            let (sender, receiver) = mpsc::channel(32);
            let port = if let Some(opening) = &mut opening {
                let binding = opening
                    .binding
                    .clone()
                    .ok_or_else(|| error("native binding missing"))?;
                if let Some(previous) = &previous {
                    super::store::choice::validate(previous, Some(&binding))?;
                }
                super::store::choice::validate(&journal, Some(&binding))?;
                let port = Arc::new(
                    NativePort::new_owned(
                        revision,
                        journal.clone(),
                        store,
                        sender,
                        resources.clone(),
                        binding,
                    )
                    .recovering(opening.store.recovery().cloned()),
                );
                opening.port = Some(port.clone());
                port
            } else {
                super::store::choice::validate(&journal, None)?;
                Arc::new(NativePort::new(
                    revision,
                    journal.clone(),
                    store,
                    sender,
                    resources.clone(),
                ))
            };
            {
                let mut state = port.state.lock().await;
                if let Some(opening) = &mut opening {
                    opening.epoch_started = true;
                }
                if opening
                    .as_ref()
                    .is_none_or(|opening| opening.store.recovery().is_none())
                {
                    port.persist(&mut state, journal.clone()).await?;
                }
                if let Some(opening) = &mut opening {
                    opening.epoch_committed = opening.store.recovery().is_none();
                }
            }
            let mut operations = resources.manifest().required_features.clone();
            let context_feature = crate::storage::checkpoint::snapshot::context::FEATURE;
            if !operations.iter().any(|feature| feature == context_feature) {
                operations.push(context_feature.into());
            }
            let caps = Capabilities {
                version: VERSION,
                core_instance_id: grant.core_instance_id.clone(),
                operations,
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
            let core = if let Some(opening) = &mut opening {
                let checked = opening
                    .binding
                    .clone()
                    .ok_or_else(|| error("native ownership missing"))?;
                let choice_journal = super::store::choice::JournalView::new(&port, &opening.store);
                let register = |core| {
                    opening.core = Some(core);
                    std::future::ready(Ok(()))
                };
                if previous.is_some() {
                    CoreSession::restore_owned_registered(
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
                        checked,
                        port.clone(),
                        choice_journal,
                        register,
                    )
                    .await?
                } else {
                    CoreSession::bind_owned_registered(
                        binding,
                        &caps,
                        app,
                        checked,
                        port.clone(),
                        choice_journal,
                        register,
                    )
                    .await?
                }
            } else if previous.is_some() {
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
            if let Some(opening) = &mut opening
                && let Some(checked) = opening.store.recovery().cloned()
            {
                opening.epoch_committed = checked.committed();
                let bound = opening
                    .binding
                    .as_ref()
                    .ok_or_else(|| error("native binding missing"))?;
                fence
                    .activate_stopped_native(&checked, bound.ownership_grant())
                    .map_err(|err| error(err.to_string()))?;
                opening.store.retire_recovery()?;
                let mut state = port.state.lock().await;
                let next = state.journal.clone();
                port.persist(&mut state, next).await?;
                checked
                    .confirm_retired()
                    .map_err(|_| error("native retirement acknowledgement missing"))?;
            }
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
            if let Some(opening) = &opening {
                opening.check()?;
            }
            port.restored();
            Ok(Self {
                core,
                port,
                resources: resources.clone(),
                receiver,
                workspace: fence.clone(),
                effects: Arc::new(RwLock::new(())),
                owned_handle: None,
            })
        }
        .await;
        if initialized.is_err() && opening.is_none() {
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

    #[cfg(all(test, unix))]
    pub(crate) fn harness_port_for_test(&self) -> Arc<dyn HarnessPort> {
        self.port.clone()
    }

    /// Assess one frozen Eval root against this authenticated handle's retained
    /// experiment route. This local read starts no model work and takes no
    /// execution lease. Current tenant-wide ResourceRead and the original
    /// credential/binding ceilings are required through delivery.
    ///
    /// # Errors
    /// Rejects Legacy, a busy owner, unsupported retained policy, unqualified
    /// external evidence, revoked authority or an exceeded bounded read budget.
    /// Built-in operational evidence does not imply a quality pass or promotion.
    pub async fn experiment_cohort(
        &self,
        model: &str,
        evidence_root: &str,
    ) -> Result<crate::core::optimization::cohort::CohortAssessment, CoreError> {
        let handle = self.owned_handle.as_ref().ok_or_else(|| {
            CoreError::rejected(
                ErrorCode::UnauthorizedScope,
                "native cohort requires an authenticated session",
            )
        })?;
        handle.cohort(model, evidence_root).await
    }

    pub async fn run(
        &mut self,
        input: TaskInput,
        approval: Arc<dyn NativeApproval>,
        cancel: CancellationToken,
    ) -> Result<SessionSnapshot, CoreError> {
        if let Some(handle) = &self.owned_handle {
            return handle.run(input, approval, cancel).await;
        }
        self.start(&format!("input_{}", uuid::Uuid::new_v4()), input)
            .await?;
        self.drive(approval, cancel).await
    }

    /// Commit an exact input before driving it. Reusing its identity checks the
    /// original request and returns its durable receipt, never a second run.
    pub async fn start(
        &self,
        operation_id: &str,
        input: TaskInput,
    ) -> Result<OperationReceipt, CoreError> {
        if self.owned_handle.is_some() {
            return Err(CoreError::rejected(
                ErrorCode::UnauthorizedScope,
                "authenticated native sessions must use the owned run path",
            ));
        }
        let revision = match self.core.operation(operation_id).await {
            Some(receipt) => receipt
                .state_revision
                .checked_sub(1)
                .ok_or_else(|| error("invalid input receipt revision"))?,
            None => self.core.head().await.state_revision,
        };
        self.core.start(operation_id, revision, input).await
    }

    /// Advance only the already-accepted run through the existing Core driver.
    pub async fn drive(
        &mut self,
        approval: Arc<dyn NativeApproval>,
        cancel: CancellationToken,
    ) -> Result<SessionSnapshot, CoreError> {
        if self.owned_handle.is_some() {
            return Err(CoreError::rejected(
                ErrorCode::UnauthorizedScope,
                "authenticated native sessions must use the owned run path",
            ));
        }
        if cancel.is_cancelled() {
            cancel_current_run(&self.core).await?;
        }
        let result = self.drive_inner(approval, cancel).await;
        if result.is_err() {
            self.port.cancel_tools();
            self.core.disconnect().await;
        }
        result
    }

    async fn drive_inner(
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
                        Some(ServerMessage::SkillConsentRequest(request)) => {
                            let (core, approval) = (core.clone(), approval.clone());
                            jobs.spawn(async move {
                                let approved = tokio::select! {
                                    biased;
                                    () = core.skill_consent_expired(&request) => return Ok(()),
                                    approved = approval.approve_skill(&request) => approved?,
                                };
                                if !core.skill_consent_pending(&request).await { return Ok(()); }
                                let result = core.answer_skill_consent(&operation("skill_answer", &request.consent.request_id), request.answer(approved)).await;
                                if let Err(error) = result
                                    && (!matches!(error.code, ErrorCode::StaleRevision | ErrorCode::StaleEpoch | ErrorCode::UnauthorizedScope)
                                        || core.skill_consent_pending(&request).await) { return Err(error); }
                                Ok(())
                            });
                        }
                        Some(ServerMessage::MaterialRequest { request_id, material_id, version }) => {
                            let resources = self.resources.clone();
                            let result = self.port.with_dispatch(|| tokio::spawn(async move {
                                resources.material(&material_id, &version).await
                            }));
                            let (material, unavailable) = match result {
                                Ok(worker) => match worker.await {
                                    Ok(Ok(material)) => (Some(material), None),
                                    Ok(Err(reason)) => (None, Some(reason)),
                                    Err(_) => (None, Some("native material worker unavailable".into())),
                                },
                                Err(_) => (None, Some("native material request no longer authorized".into())),
                            };
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
        let mut outcome = outcome;
        if driving {
            match driver.await {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => outcome = Err(error),
                Err(_) => outcome = Err(error("native core driver did not settle")),
            }
        }
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

    pub async fn close(mut self) -> Result<(), CoreError> {
        if let Some(handle) = self.owned_handle.take() {
            return handle.close().await;
        }
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
                    && err.commit_status
                        == bitrouter_ai::core::protocol::CommitStatus::NotCommitted =>
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
        port.authorize_dispatch().await?;
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
            // A dispatch rejection never calls the closure, so no worker was
            // admitted. Convert only that rejection to None; the result path
            // below durably records NotExecuted, preserving cleanup and ACK.
            port.with_dispatch(|| {
                tokio::spawn(async move { resources.execute(&command, &cancel).await })
            })
            .ok()
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

impl Drop for NativeSession {
    fn drop(&mut self) {
        if let Some(handle) = &self.owned_handle {
            handle.cancel();
        }
    }
}
