//! Adapt the Core scheduler to native workspace and Thread persistence.

mod artifact;
mod budget;
mod journal;
mod port;
mod projection;
mod scheduling;
mod tools;

use std::sync::Arc;
use std::time::Instant;

use bitrouter_ai::types::{Message, Role, Tool};
use futures::{StreamExt, stream::FuturesUnordered};
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

use super::{Agent, RunChannels, RunReport, RunStatus};
use crate::core::checkpoint::DurableHead;
use crate::core::context_router::FEATURE;
use crate::core::protocol::{
    Bind, Capabilities, ContextMode, HarnessManifest, HarnessTool, Limits, OwnershipGrant,
    RoutingSettings, ServerMessage, TaskInput, ToolEffect,
};
use crate::core::session::{CoreSession, RunStatus as CoreStatus, SessionSnapshot};

/// Published only after the native driver and all workspace workers joined.
/// Core restoration still validates the checkpoint and every artifact before
/// this state can authorize a later Thread turn.
#[derive(Clone)]
pub(crate) struct Saved {
    journal: Arc<journal::Journal>,
    task_text: String,
    projections: std::collections::VecDeque<String>,
    verification_calls: std::collections::BTreeSet<String>,
}

impl Saved {
    pub(crate) fn complete_projection(&self) -> bool {
        self.projections.is_empty()
    }

    pub(crate) fn bytes(&self) -> usize {
        self.journal.bytes()
    }

    pub(crate) fn replay(
        saved: &mut Option<Self>,
        record: &crate::store::ExecutionRecord,
    ) -> Result<Option<SessionSnapshot>, String> {
        use crate::store::ExecutionRecord;
        use base64::Engine;
        if let Some(saved) = saved {
            if let ExecutionRecord::ToolIntent { call, .. } = record
                && call.origin == crate::item::CallOrigin::Verification
            {
                saved.verification_calls.insert(call.item_id.clone());
            } else if matches!(
                record,
                ExecutionRecord::ModelRequest { .. }
                    | ExecutionRecord::ModelResponse { .. }
                    | ExecutionRecord::ModelInterrupted { .. }
                    | ExecutionRecord::ToolIntent { .. }
                    | ExecutionRecord::ToolResult { .. }
            ) && !matches!(record, ExecutionRecord::ToolResult { item_id, .. } if saved.verification_calls.contains(item_id))
            {
                let digest = crate::core::checkpoint::sha256(
                    &serde_json::to_vec(record).map_err(|error| error.to_string())?,
                );
                if saved.projections.pop_front().as_ref() != Some(&digest) {
                    return Err(
                        "native display projection identity or contents differ from Core evidence"
                            .into(),
                    );
                }
            }
            if matches!(
                record,
                ExecutionRecord::CoreCheckpoint { .. } | ExecutionRecord::CoreDispatch
            ) && !saved.projections.is_empty()
            {
                return Err(
                    "native checkpoint is missing its committed display projections".into(),
                );
            }
        }
        match record {
            ExecutionRecord::CoreArtifact {
                reference,
                offset,
                content_base64,
            } => {
                let saved = saved
                    .as_mut()
                    .ok_or("native artifact precedes Core binding")?;
                let journal = Arc::make_mut(&mut saved.journal);
                if content_base64.len() as u64 > journal.limits.input_bytes {
                    return Err("native artifact chunk exceeds input bound".into());
                }
                let bytes = base64::engine::general_purpose::STANDARD
                    .decode(content_base64)
                    .map_err(|_| "invalid native artifact encoding")?;
                journal
                    .apply_chunk(reference.clone(), *offset, &bytes)
                    .map_err(|error| error.message)?;
                Ok(None)
            }
            ExecutionRecord::CoreCheckpoint { batch, limits } => {
                limits.validate().map_err(|error| error.message)?;
                let payload = batch.decode(limits).map_err(|error| error.message)?;
                let snapshot: SessionSnapshot =
                    serde_json::from_value(payload.checkpoint.state.clone())
                        .map_err(|error| error.to_string())?;
                crate::core::context_router::validation::validate(&snapshot)
                    .map_err(|error| error.message)?;
                if saved.is_none() {
                    *saved = Some(Self {
                        journal: Arc::new(journal::Journal::new(
                            OwnershipGrant {
                                session_id: batch.identity.session_id.clone(),
                                harness_id: "native-bro".into(),
                                core_instance_id: batch.identity.core_instance_id.clone(),
                                execution_epoch: batch.identity.execution_epoch,
                            },
                            limits.clone(),
                            snapshot.manifest.artifact_quota_bytes,
                        )),
                        task_text: String::new(),
                        projections: Default::default(),
                        verification_calls: Default::default(),
                    });
                }
                let saved = saved
                    .as_mut()
                    .ok_or("native checkpoint reconstruction failed")?;
                let journal = Arc::make_mut(&mut saved.journal);
                if journal.grant.session_id != batch.identity.session_id {
                    return Err("native Core binding changed within a Thread".into());
                }
                if journal.limits != *limits {
                    if journal.grant.execution_epoch.checked_add(1)
                        != Some(batch.identity.execution_epoch)
                        || !payload
                            .events
                            .iter()
                            .any(|event| event.kind == "session.restored")
                    {
                        return Err("native capacity changed outside restoration".into());
                    }
                    journal
                        .upgrade_capacity(limits)
                        .map_err(|error| error.message)?;
                }
                if journal.grant.execution_epoch != batch.identity.execution_epoch {
                    if journal.grant.execution_epoch.checked_add(1)
                        != Some(batch.identity.execution_epoch)
                        || !payload
                            .events
                            .iter()
                            .any(|event| event.kind == "session.restored")
                    {
                        return Err("native Core epoch changed without restoration".into());
                    }
                    journal.grant.execution_epoch = batch.identity.execution_epoch;
                    journal.grant.core_instance_id = batch.identity.core_instance_id.clone();
                }
                let ack = journal.prepare(batch).map_err(|error| error.message)?;
                let projection =
                    projection::project(&snapshot, &journal.presented, &payload.events);
                for record in &projection.records {
                    saved.projections.push_back(crate::core::checkpoint::sha256(
                        &serde_json::to_vec(record).map_err(|error| error.to_string())?,
                    ));
                }
                journal.presented.extend(projection.keys);
                journal
                    .apply(batch.clone(), &ack)
                    .map_err(|error| error.message)?;
                if let Some(run) = &snapshot.run {
                    saved.task_text.clone_from(&run.input.text);
                }
                Ok(Some(snapshot))
            }
            _ => Ok(None),
        }
    }
}

impl Agent {
    pub(super) async fn run_native(
        &self,
        report: &mut RunReport,
        text: &str,
        cancel: &CancellationToken,
        channels: RunChannels,
        started: Instant,
    ) -> (RunStatus, String) {
        match self
            .drive_native(report, text, cancel, channels, started)
            .await
        {
            Ok(result) => result,
            Err(error) => (RunStatus::Failed, error),
        }
    }

    async fn drive_native(
        &self,
        report: &mut RunReport,
        text: &str,
        cancel: &CancellationToken,
        channels: RunChannels,
        started: Instant,
    ) -> Result<(RunStatus, String), String> {
        let remaining = self.config.max_steps.saturating_sub(report.steps);
        let saved = report.native.clone();
        let resumed = saved
            .as_ref()
            .and_then(|saved| {
                saved
                    .journal
                    .checkpoint
                    .as_ref()
                    .map(|batch| (batch, &saved.journal.limits))
            })
            .map(|(batch, limits)| batch.decode(limits).map_err(|error| error.message))
            .transpose()?
            .map(|payload| {
                serde_json::from_value::<SessionSnapshot>(payload.checkpoint.state)
                    .map_err(|error| error.to_string())
            })
            .transpose()?
            .filter(|state| {
                state.run.as_ref().is_some_and(|run| {
                    !matches!(
                        run.status,
                        CoreStatus::Completed | CoreStatus::Cancelled | CoreStatus::Failed
                    )
                })
            });
        if resumed.is_some() && !text.is_empty() {
            return Err("active native Core can only continue its admitted Turn".into());
        }
        if remaining == 0 && resumed.is_none() {
            return Ok((RunStatus::BoundExceeded, "step bound reached".into()));
        }
        let task_text = if text.is_empty() {
            saved
                .as_ref()
                .map(|saved| saved.task_text.clone())
                .ok_or("native continuation has no prior task")?
        } else {
            text.to_string()
        };
        let text = task_text.as_str();
        // Thread persistence splits large checkpoints across bounded store
        // pages without changing Core's exact-byte ACK or state capacity.
        let checkpoint_bytes = self.native_checkpoint_bytes;
        let limits = Limits {
            model_attempts: remaining,
            active_seconds: self
                .config
                .max_duration
                .saturating_sub(started.elapsed())
                .as_secs()
                .max(1),
            outstanding_tools: u32::try_from(self.parallel_tools)
                .unwrap_or(u32::MAX)
                .max(8),
            total_tools: self.config.max_tool_calls.saturating_sub(report.tool_calls),
            input_bytes: (128 * 1024).min(self.native_record_bytes as u64 / 4),
            checkpoint_bytes,
            unacknowledged_bytes: checkpoint_bytes.saturating_mul(2),
            ..Default::default()
        };
        let grant = OwnershipGrant {
            session_id: uuid::Uuid::new_v4().to_string(),
            harness_id: "native-bro".into(),
            core_instance_id: uuid::Uuid::new_v4().to_string(),
            execution_epoch: 1,
        };
        let mut capabilities = Capabilities {
            version: crate::core::protocol::VERSION,
            core_instance_id: grant.core_instance_id.clone(),
            operations: vec![
                FEATURE.into(),
                crate::core::context_router::NATIVE_TOOLS.into(),
            ],
            transports: vec!["native".into()],
            unsupported_features: Vec::new(),
            limits: limits.clone(),
            max_sessions: 1,
            max_host_model_attempts: limits.active_models,
        };
        let manifest_tools = self
            .declarations()
            .into_iter()
            .filter_map(|tool| match tool {
                Tool::Function {
                    name,
                    description,
                    parameters,
                    ..
                } => {
                    let effect = if crate::tools::WorkspaceTools::read_only(&name) {
                        ToolEffect::Read
                    } else if matches!(name.as_str(), "write" | "edit") {
                        ToolEffect::Write
                    } else if name == "shell" {
                        ToolEffect::Shell
                    } else {
                        ToolEffect::Unknown
                    };
                    Some(HarnessTool {
                        name,
                        description: description.unwrap_or_default(),
                        parameters,
                        effect,
                        approval_required: effect != ToolEffect::Read
                            && channels.approvals.is_some(),
                    })
                }
                _ => None,
            })
            .chain(std::iter::once(artifact::declaration()))
            .collect::<Vec<_>>();
        let manifest = HarnessManifest {
            tool_manifest_digest: HarnessManifest::digest(&manifest_tools)
                .map_err(|error| error.message)?,
            tools: manifest_tools,
            workspace_id: crate::core::checkpoint::sha256(
                self.tools.root().to_string_lossy().as_bytes(),
            ),
            workspace_revision: None,
            permission_revision: 1,
            max_tool_output_bytes: (limits.input_bytes / 2).min(8 * 1024),
            artifact_quota_bytes: 32 * 1024 * 1024,
            max_artifact_chunk_bytes: limits.input_bytes / 4,
            required_features: vec![
                FEATURE.into(),
                crate::core::context_router::NATIVE_TOOLS.into(),
            ],
        };
        let (output, mut commands) = mpsc::channel(64);
        let mut journal = saved
            .map(|saved| saved.journal.as_ref().clone())
            .unwrap_or_else(|| {
                journal::Journal::new(grant.clone(), limits.clone(), manifest.artifact_quota_bytes)
            });
        if journal.limits.checkpoint_bytes > limits.checkpoint_bytes
            || journal.limits.input_bytes > limits.input_bytes
        {
            return Err(
                "native recovery record policy is smaller than its retained Core contract".into(),
            );
        }
        if resumed.is_none() {
            let mut upgraded = journal.limits.clone();
            upgraded.checkpoint_bytes = limits.checkpoint_bytes;
            upgraded.unacknowledged_bytes = limits.unacknowledged_bytes;
            journal
                .upgrade_capacity(&upgraded)
                .map_err(|error| error.message)?;
        }
        capabilities.limits = journal.limits.clone();
        let restore = if journal.checkpoint.is_some() {
            Some(
                journal
                    .restore(&capabilities.core_instance_id, manifest.clone())
                    .map_err(|error| error.message)?,
            )
        } else {
            None
        };
        let budget = match &resumed {
            Some(state) => budget::Budget::resume(&self.config, report, state)?,
            None => budget::Budget::new(&self.config, report),
        };
        let port = Arc::new(port::Port {
            journal: tokio::sync::Mutex::new(journal),
            budget: tokio::sync::Mutex::new(budget),
            commits: channels.commits,
            events: channels.events.clone(),
            output,
            recorded: Default::default(),
            root_agent: Default::default(),
            resources: self.resources.clone(),
            stopped: CancellationToken::new(),
            cancelled: cancel.clone(),
            durable_error: Default::default(),
            launch: channels
                .control
                .as_ref()
                .map(|control| control.fence.clone())
                .unwrap_or_default(),
        });
        report.native = None;
        let core = if let Some(restore) = restore {
            CoreSession::restore(
                restore,
                &capabilities,
                self.app.clone(),
                self.caller.clone(),
                port.clone(),
            )
            .await
        } else {
            CoreSession::bind(
                Bind {
                    grant,
                    durable_head: DurableHead::default(),
                    checkpoint: None,
                    manifest: manifest.clone(),
                    limits: limits.clone(),
                },
                &capabilities,
                self.app.clone(),
                self.caller.clone(),
                port.clone(),
            )
            .await
        };
        let core = match core {
            Ok(core) => core,
            Err(error) => {
                return Err(initialization_failed(&port, None, report, text, error.message).await);
            }
        };
        let mut history = report.messages.clone();
        if resumed.is_none() && history.last() == Some(&Message::text(Role::User, text)) {
            history.pop();
        }
        if let Err(error) = core
            .seed_native_context(
                history,
                vec![format!(
                    "{}\n\n{}",
                    self.config.instructions,
                    crate::harness::instructions::POLICY
                )],
                report.context_version.saturating_sub(1),
            )
            .await
        {
            return Err(
                initialization_failed(&port, Some(&core), report, text, error.message).await,
            );
        }
        let run_id = if let Some(state) = &resumed {
            state
                .run
                .as_ref()
                .ok_or("native continuation lost its run")?
                .run_id
                .clone()
        } else {
            let input = TaskInput {
                max_concurrent_subagents: Some(4),
                text: text.into(),
                model: self.config.model.clone(),
                effort: self.config.effort.map(|effort| effort.to_string()),
                max_output_tokens: self.config.max_output_tokens,
                context_limit_bytes: Some(self.config.max_context_bytes as u64),
                routing: RoutingSettings {
                    model: self.config.model_mode,
                    context: ContextMode::Auto,
                },
                discardable_history: None,
                acceptance_criteria: Vec::new(),
                required_materials: Vec::new(),
                verification: None,
                limits: Some(limits),
            };
            let receipt = core
                .start(
                    &uuid::Uuid::new_v4().to_string(),
                    core.head().await.state_revision,
                    input,
                )
                .await;
            let receipt = match receipt {
                Ok(receipt) => receipt,
                Err(error) => {
                    return Err(initialization_failed(
                        &port,
                        Some(&core),
                        report,
                        text,
                        error.message,
                    )
                    .await);
                }
            };
            let run_id = receipt
                .assigned_ids
                .get("run_id")
                .cloned()
                .ok_or("native run identity is missing");
            match run_id {
                Ok(run_id) => run_id,
                Err(error) => {
                    return Err(initialization_failed(
                        &port,
                        Some(&core),
                        report,
                        text,
                        error.into(),
                    )
                    .await);
                }
            }
        };
        let mut workers = FuturesUnordered::new();
        let effects = Arc::new(tokio::sync::RwLock::new(()));
        let mut task_tokens = std::collections::BTreeMap::new();
        let mut scheduling = scheduling::Tools::default();
        let mut cancelled = false;
        if let Err(error) = apply_inputs(channels.control.as_ref(), &core).await {
            return Err(initialization_failed(&port, Some(&core), report, text, error).await);
        }
        let mut driving = Some({
            let core = core.clone();
            tokio::spawn(async move { core.drive().await })
        });
        let mut needs_drive = false;
        let outcome = async { loop {
            if driving.is_none() && needs_drive {
                let core = core.clone();
                driving = Some(tokio::spawn(async move { core.drive().await }));
                needs_drive = false;
            }
            while let Some((dispatch, skip)) = scheduling.next(self.parallel_tools) {
                let token = cancel.child_token();
                task_tokens.insert(dispatch.invocation_id.clone(), token.clone());
                let agent = self.clone();
                let core = core.clone();
                let port = port.clone();
                let approvals = channels.approvals.clone();
                let effects = effects.clone();
                workers.push(tokio::spawn(async move {
                    let outcome = tools::execute(agent, core, port, (dispatch.clone(), skip), token, approvals, effects).await;
                    (dispatch, outcome)
                }));
            }
            tokio::select! {
                biased;
                _ = async { match &channels.control {
                    Some(control) => control.native.changed().await,
                    None => std::future::pending::<()>().await,
                } }, if !cancelled => {
                    apply_inputs(channels.control.as_ref(), &core).await?;
                    needs_drive = true;
                }
                _ = cancel.cancelled(), if !cancelled => {
                    cancelled = true;
                    loop {
                        let revision = core.head().await.state_revision;
                        match core.cancel_run(&uuid::Uuid::new_v4().to_string(), revision, &run_id).await {
                            Err(error) if error.code == crate::core::protocol::ErrorCode::StaleRevision => continue,
                            result => { result.map_err(|error| error.message)?; break; }
                        }
                    }
                    needs_drive = true;
                }
                command = commands.recv() => {
                    match command.ok_or("native command channel closed")? {
                        ServerMessage::ToolExecute(dispatch) => {
                            scheduling.push(*dispatch);
                        }
                        ServerMessage::ToolCancel { invocation_id, .. } => {
                            if let Some(token) = task_tokens.get(&invocation_id) { token.cancel(); }
                        }
                        ServerMessage::Error(error) => break Err(error.message),
                        other => break Err(format!("unsupported native Core command: {other:?}")),
                    }
                }
                done = workers.next(), if !workers.is_empty() => {
                    match done {
                        Some(Ok((dispatch, Ok(failed)))) => { scheduling.finished(&dispatch, failed); }
                        Some(Ok((_, Err(error)))) => break Err(error),
                        _ => { report.unknown_effect = true; break Err("native tool worker lost".into()); }
                    }
                    needs_drive = true;
                }
                done = async { match &mut driving { Some(run) => Some(run.await), None => std::future::pending().await } } => {
                    driving = None;
                    match done {
                        Some(Ok(Ok(snapshot))) => {
                            if terminal(&snapshot) && workers.is_empty() && scheduling.is_empty() { break Ok(snapshot); }
                        }
                        Some(Ok(Err(error))) => break Err(error.message),
                        _ => break Err("native Core driver lost".into()),
                    }
                }
            }
        } }.await;
        if outcome.is_err() {
            port.stopped.cancel();
            core.disconnect().await;
            for token in task_tokens.values() {
                token.cancel();
            }
        }
        if let Some(driver) = driving {
            let _ = driver.await;
        }
        while let Some(result) = workers.next().await {
            if !matches!(result, Ok((_, Ok(_)))) {
                report.unknown_effect = true;
            }
        }
        let snapshot = core.snapshot().await;
        report.unknown_effect |= port
            .durable_error
            .load(std::sync::atomic::Ordering::Acquire)
            || (outcome.is_err() && !terminal(&snapshot));
        update_report(report, &snapshot);
        let budget = port.budget.lock().await;
        budget.update_report(report, &snapshot);
        let budget_reason = budget.reason.clone();
        drop(budget);
        report.events.extend(port.recorded.lock().await.drain(..));
        core.disconnect().await;
        if snapshot.run.as_ref().is_some_and(|run| {
            matches!(
                run.status,
                CoreStatus::Completed | CoreStatus::Failed | CoreStatus::Cancelled
            )
        }) && !report.unknown_effect
        {
            report.native = Some(Saved {
                journal: Arc::new(port.journal.lock().await.clone()),
                task_text,
                projections: Default::default(),
                verification_calls: Default::default(),
            });
        }
        if let Some(reason) = budget_reason {
            return Ok((RunStatus::BoundExceeded, reason));
        }
        outcome?;
        let root_reason = snapshot
            .root_turn()
            .and_then(|turn| turn.terminal_reason.clone());
        let run = snapshot.run.ok_or("native run is absent")?;
        Ok((
            match run.status {
                _ if cancel.is_cancelled() && !report.unknown_effect => RunStatus::Cancelled,
                _ if run.resource_error.is_some() && !report.unknown_effect => {
                    RunStatus::BoundExceeded
                }
                _ if run.status == CoreStatus::Failed
                    && run.model_attempts >= run.limits.model_attempts
                    && !report.unknown_effect =>
                {
                    RunStatus::BoundExceeded
                }
                CoreStatus::Completed => RunStatus::Completed,
                CoreStatus::Cancelled => RunStatus::Cancelled,
                _ => RunStatus::Failed,
            },
            run.resource_error
                .as_ref()
                .map(|error| error.message.clone())
                .or(root_reason)
                .or(run.terminal_reason)
                .unwrap_or_else(|| format!("native Core run {:?}", run.status)),
        ))
    }
}

async fn initialization_failed(
    port: &port::Port,
    core: Option<&CoreSession>,
    report: &mut RunReport,
    task_text: &str,
    error: String,
) -> String {
    if let Some(core) = core {
        core.disconnect().await;
    }
    if port.stopped.is_cancelled() {
        report.unknown_effect = true;
        return error;
    }
    let mut journal = port.journal.lock().await.clone();
    let Some(batch) = &journal.checkpoint else {
        return error;
    };
    let snapshot = batch.decode(&journal.limits).ok().and_then(|payload| {
        serde_json::from_value::<SessionSnapshot>(payload.checkpoint.state).ok()
    });
    if snapshot.as_ref().is_some_and(|snapshot| {
        snapshot.run.as_ref().is_none_or(|run| {
            matches!(
                run.status,
                CoreStatus::Completed | CoreStatus::Failed | CoreStatus::Cancelled
            )
        }) && snapshot
            .agents
            .values()
            .filter_map(|agent| agent.turn.as_ref())
            .flat_map(|turn| &turn.invocations)
            .all(|call| {
                call.result.as_ref().is_some_and(|result| {
                    result.status != crate::core::protocol::ToolOutcome::EffectUnknown
                })
            })
    }) {
        // A rejected restore may have allocated an epoch without committing it.
        // The retained checkpoint, not that attempted grant, owns the next epoch.
        journal.grant.execution_epoch = batch.identity.execution_epoch;
        journal.grant.core_instance_id = batch.identity.core_instance_id.clone();
        report.native = Some(Saved {
            journal: Arc::new(journal),
            task_text: task_text.into(),
            projections: Default::default(),
            verification_calls: Default::default(),
        });
    } else {
        report.unknown_effect = true;
    }
    error
}

fn terminal(snapshot: &SessionSnapshot) -> bool {
    snapshot.run.as_ref().is_some_and(|run| {
        matches!(
            run.status,
            CoreStatus::Completed
                | CoreStatus::Failed
                | CoreStatus::Cancelled
                | CoreStatus::RecoveryRequired
        )
    })
}

async fn apply_inputs(
    control: Option<&crate::control::TurnControl>,
    core: &CoreSession,
) -> Result<(), String> {
    let Some(control) = control else {
        return Ok(());
    };
    for (id, text) in control.native.take() {
        loop {
            let snapshot = core.snapshot().await;
            let turn = snapshot
                .root_turn()
                .ok_or("native steering has no root task")?;
            if let Some(receipt) = snapshot.steering.get(&id) {
                if receipt.text != text
                    || receipt.run_id != turn.run_id
                    || receipt.agent_turn_id != turn.agent_turn_id
                {
                    return Err("recovered native steering differs from its admitted target".into());
                }
                break;
            }
            let revision = core.head().await.state_revision;
            match core
                .steer(
                    &id,
                    revision,
                    &turn.run_id,
                    &turn.agent_turn_id,
                    text.clone(),
                )
                .await
            {
                Err(error) if error.code == crate::core::protocol::ErrorCode::StaleRevision => {
                    continue;
                }
                result => {
                    result.map_err(|error| error.message)?;
                    break;
                }
            }
        }
    }
    Ok(())
}

fn update_report(report: &mut RunReport, state: &SessionSnapshot) {
    if let Some(root) = state.agents.get(&state.agent_id) {
        report.messages = root.history.clone();
        report.context_version = root.context_revision;
    }
    if let Some(run) = &state.run {
        report.final_answer = run.final_answer.clone();
        report.unknown_effect |= run.status == CoreStatus::RecoveryRequired;
    }
}
