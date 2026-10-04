//! Restore only authenticated durable state. No provider or workspace execution
//! occurs until the reconciled snapshot is acknowledged under the current grant.

use super::*;
use crate::core::protocol::{ArtifactRef, Restore, ToolObservation, ToolStatus};

impl CoreSession {
    /// The host must authenticate the durable harness and current ownership
    /// grant before calling. `previous_owner_stopped` attests that the previous
    /// scheduler and its provider I/O have been stopped/reconciled, including
    /// when replacing a process under the same core instance and epoch.
    pub async fn restore(
        request: Restore,
        capabilities: &Capabilities,
        app: Arc<App>,
        caller: CallerContext,
        harness: Arc<dyn HarnessPort>,
    ) -> Result<Self, CoreError> {
        Self::restore_with_headers(
            request,
            capabilities,
            app,
            caller,
            http::HeaderMap::new(),
            harness,
        )
        .await
    }

    /// Restore with newly authenticated volatile credentials. Durable snapshots
    /// never restore credentials or establish the caller's authority.
    pub async fn restore_with_headers(
        request: Restore,
        capabilities: &Capabilities,
        app: Arc<App>,
        caller: CallerContext,
        request_headers: http::HeaderMap,
        harness: Arc<dyn HarnessPort>,
    ) -> Result<Self, CoreError> {
        Self::restore_registered(
            request,
            capabilities,
            app,
            caller,
            request_headers,
            harness,
            |_| std::future::ready(Ok(())),
        )
        .await
    }

    /// Publish a prepared session to the host before its first checkpoint wait.
    /// Registration must keep it unavailable for execution until binding succeeds;
    /// retain the handle on errors to reconcile the original pending batch.
    pub async fn restore_registered<Register, Registered>(
        request: Restore,
        capabilities: &Capabilities,
        app: Arc<App>,
        caller: CallerContext,
        request_headers: http::HeaderMap,
        harness: Arc<dyn HarnessPort>,
        register: Register,
    ) -> Result<Self, CoreError>
    where
        Register: FnOnce(CoreSession) -> Registered + Send,
        Registered: std::future::Future<Output = Result<(), CoreError>> + Send,
    {
        let handoff = Instant::now();
        if !request.previous_owner_stopped {
            return Err(reject(
                ErrorCode::RecoveryRequired,
                "previous scheduler and provider I/O are not reconciled",
            ));
        }
        let mut state = restore_snapshot(&request, capabilities, harness.as_ref()).await?;
        let workspace_changed =
            state.manifest.workspace_revision != request.binding.manifest.workspace_revision;
        state.manifest = request.binding.manifest.clone();
        let sent_tools = reconcile_tools(&mut state, &request, workspace_changed)?;
        responses::validate(&state, &request.binding)?;
        recovery_time::reconcile(&mut state, &request)?;
        let outputs = resume_model_steps(&mut state);
        let initialization = PendingInitialization {
            base_revision: request.binding.durable_head.state_revision,
            kind: "session.restored",
            payload: json!({"previous_owner_stopped":true,"tool_observations":request.tools,
                "active_time":request.active_time,"workspace_changed":workspace_changed,
                "interrupted_steps":state.agents.values().filter_map(|agent| agent.turn.as_ref())
                    .flat_map(|turn| &turn.steps).filter(|step| step.interrupted)
                    .map(|step| &step.step_id).collect::<Vec<_>>()}),
        };
        let binding = request.binding;
        let active_ms = state.run.as_ref().map_or(0, |run| run.active_ms);
        let activity = Activity::handoff(active_ms, tool_status::activity_ids(&state), handoff);
        let session = Self {
            shared: Arc::new(Shared {
                live: Mutex::new(LiveSession {
                    state,
                    initialization: Some(initialization),
                    gate: CommitGate::new(
                        binding.grant,
                        binding.durable_head,
                        binding.limits.clone(),
                    )?,
                    pending: None,
                    sent_tools,
                    unresolved_tool_deliveries: BTreeSet::new(),
                    cancelled_tools: BTreeSet::new(),
                    sent_materials: BTreeSet::new(),
                    provisional_blocks: BTreeSet::new(),
                    provisional_steering: BTreeMap::new(),
                    disconnected: CancellationToken::new(),
                    connection_generation: 0,
                    activity,
                    model_controls: Vec::new(),
                    provider_evidence: Default::default(),
                    reconnecting: false,
                    budget_watching: false,
                    restoration_started: Some(handoff),
                    restoration_stops: BTreeMap::new(),
                }),
                commits: Mutex::new(()),
                driver: Mutex::new(()),
                inputs: Mutex::new(()),
                app,
                caller,
                request_headers,
                harness,
                limits: binding.limits,
                capabilities: capabilities.clone(),
                changed: Notify::new(),
                steering_changed: Notify::new(),
                budget_changed: Arc::new(Notify::new()),
            }),
        };
        register(session.clone()).await?;
        session.initialize().await?;
        for (agent_id, step_id, request_id, output) in outputs {
            let state = session.snapshot().await;
            if state
                .agents
                .get(&agent_id)
                .and_then(|agent| agent.turn.as_ref())
                .is_some_and(|turn| turn.status == AgentStatus::RecoveryRequired)
            {
                continue;
            }
            if let Err(error) = session
                .apply_output(&agent_id, &step_id, &request_id, &output)
                .await
            {
                if error.commit_status == CommitStatus::NotCommitted && session.can_progress().await
                {
                    session
                        .fail(&agent_id, &error.message)
                        .await
                        .map_err(committed_restore_error)?;
                } else {
                    return Err(committed_restore_error(error));
                }
            }
        }
        restoration_activity::finish(&session)
            .await
            .map_err(committed_restore_error)?;
        Ok(session)
    }

    /// Registration can be interrupted before a batch exists. Preserve its
    /// original event until it is acknowledged, including across reconnects.
    pub(super) async fn initialize(&self) -> Result<(), CoreError> {
        let pending = {
            let mut live = self.shared.live.lock().await;
            let Some(pending) = live.initialization.clone() else {
                return Ok(());
            };
            if live.gate.head().state_revision > pending.base_revision {
                live.initialization = None;
                return Ok(());
            }
            pending
        };
        if pending.kind == "session.restored" {
            self.shared
                .harness
                .observe_restoration(restoration_activity::RestorationActivity::new(self))
                .await?;
        }
        self.transition(pending.kind, |_, _| Ok(pending.payload))
            .await?;
        self.shared.live.lock().await.initialization = None;
        Ok(())
    }
}

fn committed_restore_error(mut error: CoreError) -> CoreError {
    // The restore checkpoint is already acknowledged. A rejected replay/stop
    // proposal or failed drain cannot undo it. Preserve uncertainty if a later
    // batch was submitted without confirmation.
    if error.commit_status == CommitStatus::NotCommitted {
        error.commit_status = CommitStatus::Committed;
    }
    error
}

async fn restore_snapshot(
    request: &Restore,
    capabilities: &Capabilities,
    harness: &dyn HarnessPort,
) -> Result<SessionSnapshot, CoreError> {
    let binding = &request.binding;
    binding.grant.validate()?;
    binding.durable_head.validate()?;
    if binding.durable_head.state_revision.checked_add(1).is_none()
        || binding.durable_head.event_seq.checked_add(1).is_none()
    {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "durable sequence exhausted",
        ));
    }
    binding.manifest.validate(capabilities, &binding.limits)?;
    if binding.grant.core_instance_id != capabilities.core_instance_id {
        return Err(reject(
            ErrorCode::UnauthorizedScope,
            "grant names a different core instance",
        ));
    }
    let checkpoint = binding.checkpoint.as_ref().ok_or_else(|| {
        reject(
            ErrorCode::CheckpointConflict,
            "restoration requires a committed checkpoint",
        )
    })?;
    // Bound the whole envelope before decoding a chain of full snapshots.
    let bytes = serde_json::to_vec(request).map_err(json_error)?.len() as u64;
    if bytes > binding.limits.unacknowledged_bytes {
        return Err(reject(
            ErrorCode::LimitExceeded,
            "restore envelope exceeds durable control bound",
        ));
    }
    let mut head = None::<DurableHead>;
    let mut owner = None::<String>;
    let mut identities = BTreeSet::new();
    let mut payloads = Vec::new();
    for batch in std::iter::once(checkpoint).chain(&request.journal_tail) {
        let payload = batch.decode(&binding.limits)?;
        if batch.identity.session_id != binding.grant.session_id {
            return Err(reject(
                ErrorCode::UnauthorizedScope,
                "restore chain belongs to another session",
            ));
        }
        if !identities.insert(batch.identity.batch_id.clone()) {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "duplicate batch in restore chain",
            ));
        }
        if batch.identity.execution_epoch > binding.grant.execution_epoch {
            return Err(reject(
                ErrorCode::StaleEpoch,
                "restore grant is older than durable history",
            ));
        }
        if let Some(previous) = &head
            && (payload.base_state_revision != previous.state_revision
                || payload.base_event_seq != previous.event_seq
                || batch.identity.execution_epoch < previous.execution_epoch
                || (batch.identity.execution_epoch == previous.execution_epoch
                    && owner.as_ref() != Some(&batch.identity.core_instance_id)))
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "restore chain has a gap or conflicting ownership",
            ));
        }
        head = Some(CheckpointAck::for_batch(batch, &payload).head());
        owner = Some(batch.identity.core_instance_id.clone());
        payloads.push((batch, payload));
    }
    if head.as_ref() != Some(&binding.durable_head) {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "restore chain does not reach the authenticated durable head",
        ));
    }
    if binding.grant.execution_epoch == binding.durable_head.execution_epoch
        && owner.as_ref() != Some(&binding.grant.core_instance_id)
    {
        return Err(reject(
            ErrorCode::StaleEpoch,
            "a different owner requires a new epoch",
        ));
    }
    // Establish the authenticated chain/head before artifact I/O. A rejected
    // cross-session batch must not cause a read through the current host port.
    let mut final_payload = None;
    let mut releases = BTreeMap::new();
    let mut activity_history = None;
    let mut resource_history = budget::ResourceHistory::default();
    let mut response_history = None;
    let mut wait_output_history = BTreeMap::new();
    let mut auxiliary_output_history = BTreeMap::new();
    for (batch, mut payload) in payloads {
        archive::hydrate(&mut payload, harness, &binding.limits).await?;
        release::validate_history(&payload, &mut releases)?;
        budget::validate_history(&payload, &mut resource_history)?;
        responses::validate_history(&payload, &mut response_history)?;
        wait_output::validate_history(&payload, &mut wait_output_history)?;
        auxiliary_output::validate_history(&payload, &mut auxiliary_output_history)?;
        recovery_time::validate_history(
            &payload,
            CheckpointAck::for_batch(batch, &payload).head(),
            &mut activity_history,
        )?;
        final_payload = Some(payload);
    }
    let payload = final_payload
        .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "empty restore chain"))?;
    let state: SessionSnapshot =
        serde_json::from_value(payload.checkpoint.state).map_err(|_| {
            reject(
                ErrorCode::CheckpointConflict,
                "checkpoint does not contain a supported session snapshot",
            )
        })?;
    validate_snapshot(&state, binding, capabilities)?;
    let available = artifact_map(request.available_artifacts.iter().cloned())?;
    let declared = artifact_map(payload.checkpoint.artifact_refs)?;
    for reference in artifacts(&state)?.values() {
        if declared.get(&reference.artifact_id) != Some(reference) {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "snapshot artifact was omitted from checkpoint dependencies",
            ));
        }
    }
    for reference in declared.values() {
        if available.get(&reference.artifact_id) != Some(reference) {
            return Err(reject(
                ErrorCode::ArtifactUnavailable,
                "required checkpoint artifact is not durably available",
            ));
        }
    }
    if binding.manifest.workspace_id != state.manifest.workspace_id
        || binding.manifest.permission_revision < state.manifest.permission_revision
    {
        return Err(reject(
            ErrorCode::UnauthorizedScope,
            "restored workspace identity or permissions regressed",
        ));
    }
    Ok(state)
}

fn validate_snapshot(
    state: &SessionSnapshot,
    binding: &Bind,
    caps: &Capabilities,
) -> Result<(), CoreError> {
    if state.session_id != binding.grant.session_id {
        return Err(reject(
            ErrorCode::UnauthorizedScope,
            "snapshot belongs to another session",
        ));
    }
    state.manifest.validate(caps, &binding.limits)?;
    release::validate(state, binding)?;
    root_queue::validate(state, &binding.limits)?;
    budget::validate(state)?;
    model_output::reserved(state)?;
    wait_output::validate(state)?;
    auxiliary_output::reserved(state)?;
    responses::validate(state, binding)?;
    steering::validate(state, &binding.limits, binding.durable_head.state_revision)?;
    let root = state
        .agents
        .get(&state.agent_id)
        .ok_or_else(|| reject(ErrorCode::CheckpointConflict, "snapshot root is absent"))?;
    if root.parent_id.is_some()
        || root.depth != 0
        || state.agents.len() > binding.limits.agents as usize
    {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "snapshot agent tree is invalid",
        ));
    }
    if let Some(run) = &state.run {
        validate_id(&run.run_id)?;
        validate_id(&run.agent_turn_id)?;
        run.limits.within(&binding.limits)?;
        if run
            .input
            .max_concurrent_subagents
            .is_some_and(|maximum| maximum >= run.limits.agents)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "restored descendant limit exceeds retained agent capacity",
            ));
        }
        if run.input.max_concurrent_subagents.is_some_and(|maximum| {
            state
                .agents
                .values()
                .filter(|agent| {
                    agent.parent_id.is_some()
                        && agent.turn.as_ref().is_some_and(|turn| {
                            turn.run_id == run.run_id && !turn.status.terminal()
                        })
                })
                .count()
                > maximum as usize
        }) {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "restored active descendants exceed the frozen limit",
            ));
        }
        if root
            .turn
            .as_ref()
            .is_none_or(|turn| turn.run_id != run.run_id || turn.agent_turn_id != run.agent_turn_id)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "root run and turn differ",
            ));
        }
    }
    let mut ids = BTreeSet::new();
    for (key, agent) in &state.agents {
        validate_id(key)?;
        if key != &agent.agent_id || agent.depth > binding.limits.child_depth {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "snapshot agent identity is invalid",
            ));
        }
        if key != &state.agent_id
            && agent
                .parent_id
                .as_ref()
                .and_then(|id| state.agents.get(id))
                .is_none_or(|parent| parent.depth.checked_add(1) != Some(agent.depth))
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "snapshot parent or depth is invalid",
            ));
        }
        let Some(turn) = &agent.turn else { continue };
        if turn
            .history_start
            .is_some_and(|start| start > agent.history.len())
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "history boundary is outside retained context",
            ));
        }
        for (index, step) in turn.steps.iter().enumerate() {
            unique(&mut ids, &step.step_id)?;
            if (!step.settled && index + 1 != turn.steps.len())
                || step.input_state_revision > binding.durable_head.state_revision
            {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "model step sequence is invalid",
                ));
            }
            for attempt in &step.attempts {
                unique(&mut ids, &attempt.attempt_id)?;
            }
        }
        for invocation in &turn.invocations {
            let limits = tool_payloads::limits(
                invocation,
                &binding.limits,
                state.run.as_ref(),
                turn.input.limits.as_ref(),
            )?;
            for observation in invocation
                .tool_observations
                .values()
                .chain(invocation.recovery_observation.iter())
                .chain(&invocation.prior_recovery_observations)
            {
                limits.validate_observation(observation)?;
            }
            tool_status::validate(
                invocation,
                &state.operations,
                binding.durable_head.state_revision,
            )?;
            let call = &invocation.dispatch;
            unique(&mut ids, &call.invocation_id)?;
            unique(&mut ids, &call.attempt_id)?;
            if call.agent_id != *key
                || call.agent_turn_id != turn.agent_turn_id
                || call.run_id != turn.run_id
                || call.workspace_id != state.manifest.workspace_id
                || call.execution_epoch > binding.durable_head.execution_epoch
                || call.authorizing_event_seq > binding.durable_head.event_seq
                || !turn
                    .steps
                    .iter()
                    .any(|step| step.step_id == call.step_id && step.settled)
                || (invocation.consumed && invocation.result.is_none())
            {
                return Err(reject(
                    ErrorCode::CheckpointConflict,
                    "tool intent has invalid ownership or authorization",
                ));
            }
            if let Some(result) = &invocation.result {
                tool_payloads::validate_result(invocation, result, limits)?;
            }
            if let Some(previous) = &invocation.prior_uncertain_result {
                tool_payloads::validate_result(invocation, previous, limits)?;
                if previous.status != ToolOutcome::EffectUnknown
                    || invocation
                        .result
                        .as_ref()
                        .is_none_or(|result| result.status == ToolOutcome::EffectUnknown)
                {
                    return Err(reject(
                        ErrorCode::CheckpointConflict,
                        "invalid reconciled tool evidence",
                    ));
                }
            }
        }
    }
    for (key, receipt) in &state.operations {
        if key != &receipt.operation_id
            || receipt.state_revision > binding.durable_head.state_revision
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "operation receipt exceeds durable history",
            ));
        }
    }
    Ok(())
}

fn unique(ids: &mut BTreeSet<String>, id: &str) -> Result<(), CoreError> {
    validate_id(id)?;
    if !ids.insert(id.into()) {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "duplicate execution identity",
        ));
    }
    Ok(())
}

fn reconcile_tools(
    state: &mut SessionSnapshot,
    request: &Restore,
    workspace_changed: bool,
) -> Result<BTreeSet<String>, CoreError> {
    let mut observations = BTreeMap::<String, &ToolObservation>::new();
    let mut results = BTreeMap::<String, &ToolResult>::new();
    for observation in &request.tools {
        if observations
            .insert(observation.invocation_id.clone(), observation)
            .is_some()
        {
            return Err(reject(
                ErrorCode::OperationConflict,
                "duplicate tool reconciliation",
            ));
        }
    }
    for result in &request.results {
        if results
            .insert(result.invocation_id.clone(), result)
            .is_some()
        {
            return Err(reject(
                ErrorCode::OperationConflict,
                "duplicate restored result",
            ));
        }
    }
    let available = artifact_map(request.available_artifacts.iter().cloned())?;
    let mut sent = BTreeSet::new();
    let mut invalidate_workspace = false;
    for agent in state.agents.values_mut() {
        let Some(turn) = &mut agent.turn else {
            continue;
        };
        turn.cancellation_requested |= turn.status == AgentStatus::Cancelling;
        let mut uncertain = false;
        let mut reconciled = false;
        for call in &mut turn.invocations {
            let limits = tool_payloads::limits(
                call,
                &request.binding.limits,
                state.run.as_ref(),
                turn.input.limits.as_ref(),
            )?;
            // Upgrade legacy payload limits while the original run policy is
            // available. An absent artifact bound stays absent: already
            // dispatched work did not promise the new body allowance.
            call.dispatch.result_limits = Some(limits);
            let invocation_id = call.dispatch.invocation_id.clone();
            let observation = observations.remove(&invocation_id);
            let result = results.remove(&invocation_id);
            if let Some(observation) = observation {
                limits.validate_observation(observation)?;
                tool_status::validate_restored_phase(call, observation, &state.operations)?;
                if observation.attempt_id != call.dispatch.attempt_id {
                    return Err(reject(
                        ErrorCode::InvalidToolResult,
                        "tool observation names a different attempt",
                    ));
                }
                ensure_artifacts(&observation.evidence, &available)?;
                if call.result.is_some()
                    && matches!(
                        observation.status,
                        ToolStatus::NotStarted | ToolStatus::WaitingApproval | ToolStatus::Running
                    )
                {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "observation conflicts with committed tool outcome",
                    ));
                }
                if let Some(previous) = &call.recovery_observation
                    && !call.prior_recovery_observations.contains(previous)
                {
                    call.prior_recovery_observations.push(previous.clone());
                }
                call.recovery_observation = Some(observation.clone());
                call.recovery_observation_revision = request
                    .binding
                    .durable_head
                    .state_revision
                    .checked_add(1)
                    .ok_or_else(|| reject(ErrorCode::LimitExceeded, "state revision exhausted"))?;
            }
            if let Some(result) = result {
                tool_payloads::validate_result(call, result, limits)?;
                ensure_artifacts(&result.evidence, &available)?;
                if let Some(previous) = &call.result
                    && previous != result
                {
                    if previous.status != ToolOutcome::EffectUnknown
                        || result.status == ToolOutcome::EffectUnknown
                        || call.consumed
                        || call.prior_uncertain_result.is_some()
                    {
                        return Err(reject(
                            ErrorCode::OperationConflict,
                            "restored result conflicts with committed outcome",
                        ));
                    }
                    call.prior_uncertain_result = Some(previous.clone());
                    reconciled = true;
                }
                if observation.is_some_and(|observation| {
                    matches!(
                        observation.status,
                        ToolStatus::NotStarted | ToolStatus::WaitingApproval | ToolStatus::Running
                    ) || (observation.status == ToolStatus::EffectUnknown
                        && result.status != ToolOutcome::EffectUnknown)
                }) {
                    return Err(reject(
                        ErrorCode::OperationConflict,
                        "tool outcome conflicts with running or unstarted observation",
                    ));
                }
                if call.result.is_none() {
                    reconciled = true;
                    invalidate_workspace |= result.status == ToolOutcome::EffectUnknown
                        || (call.effect != super::super::protocol::ToolEffect::Read
                            && !matches!(
                                result.status,
                                ToolOutcome::NotExecuted | ToolOutcome::Denied
                            ));
                }
                // A later live result remains blocked after effect_unknown.
                // Explicit authenticated restoration confirms that outcome.
                reconciled |= result.status != ToolOutcome::EffectUnknown
                    && tool_status::observed(call, ToolStatus::EffectUnknown);
                call.result = Some(result.clone());
            }
            if let Some(result) = &call.result {
                uncertain |= result.status == ToolOutcome::EffectUnknown;
                sent.insert(invocation_id);
                continue;
            }
            match observation.map(|observation| observation.status) {
                Some(ToolStatus::NotStarted) => {
                    reconciled = true;
                    if workspace_changed {
                        call.result = Some(ToolResult {
                            invocation_id: invocation_id.clone(),
                            attempt_id: call.dispatch.attempt_id.clone(),
                            status: ToolOutcome::Denied,
                            output: String::new(),
                            evidence: Vec::new(),
                            workspace_revision: None,
                        });
                        sent.insert(invocation_id);
                        continue;
                    }
                    call.dispatch.execution_epoch = request.binding.grant.execution_epoch;
                    call.dispatch.authorizing_event_seq = request
                        .binding
                        .durable_head
                        .event_seq
                        .checked_add(1)
                        .ok_or_else(|| {
                            reject(ErrorCode::LimitExceeded, "event sequence exhausted")
                        })?;
                }
                Some(ToolStatus::WaitingApproval | ToolStatus::Running) => {
                    reconciled = true;
                    sent.insert(invocation_id);
                }
                _ => {
                    // Missing evidence or merely stopped execution does not
                    // establish that a write or shell command never happened.
                    uncertain = true;
                    sent.insert(invocation_id);
                }
            }
        }
        if uncertain {
            turn.status = AgentStatus::RecoveryRequired;
        } else if reconciled && turn.status == AgentStatus::RecoveryRequired {
            turn.status = if turn.cancellation_requested
                || state
                    .run
                    .as_ref()
                    .is_some_and(|run| run.cancellation.is_some())
            {
                AgentStatus::Cancelling
            } else {
                AgentStatus::WaitingTool
            };
        }
    }
    if !observations.is_empty() || !results.is_empty() {
        return Err(reject(
            ErrorCode::InvalidToolResult,
            "restore evidence names an unknown invocation",
        ));
    }
    if invalidate_workspace {
        state.manifest.workspace_revision = None;
    }
    Ok(sent)
}

fn ensure_artifacts(
    references: &[ArtifactRef],
    available: &BTreeMap<String, ArtifactRef>,
) -> Result<(), CoreError> {
    for reference in references {
        if available.get(&reference.artifact_id) != Some(reference) {
            return Err(reject(
                ErrorCode::ArtifactUnavailable,
                "tool reconciliation evidence is not durably available",
            ));
        }
    }
    Ok(())
}

pub(super) fn artifact_map(
    references: impl IntoIterator<Item = ArtifactRef>,
) -> Result<BTreeMap<String, ArtifactRef>, CoreError> {
    let mut artifacts = BTreeMap::new();
    for reference in references {
        validate_id(&reference.artifact_id)?;
        super::super::checkpoint::validate_digest(&reference.sha256)?;
        if artifacts
            .insert(reference.artifact_id.clone(), reference.clone())
            .is_some_and(|previous| previous != reference)
        {
            return Err(reject(
                ErrorCode::CheckpointConflict,
                "artifact identity has conflicting content references",
            ));
        }
    }
    Ok(artifacts)
}

pub(super) fn artifacts(
    state: &SessionSnapshot,
) -> Result<BTreeMap<String, ArtifactRef>, CoreError> {
    let turns = || {
        state
            .agents
            .values()
            .filter_map(|agent| agent.turn.as_ref())
    };
    artifact_map(
        turns()
            .flat_map(|turn| &turn.invocations)
            .flat_map(|call| {
                call.result
                    .iter()
                    .flat_map(|result| &result.evidence)
                    .chain(
                        call.prior_uncertain_result
                            .iter()
                            .flat_map(|result| &result.evidence),
                    )
                    .chain(
                        call.recovery_observation
                            .iter()
                            .filter(|_| state.recovery_archive.is_none())
                            .flat_map(|observation| &observation.evidence),
                    )
                    .chain(
                        call.tool_observations
                            .values()
                            .flat_map(|observation| &observation.evidence),
                    )
                    .chain(
                        call.prior_recovery_observations
                            .iter()
                            .filter(|_| state.recovery_archive.is_none())
                            .flat_map(|observation| &observation.evidence),
                    )
            })
            .cloned()
            .chain(state.recovery_archive.iter().cloned())
            .chain(
                state
                    .signals
                    .materials
                    .values()
                    .filter(|material| material.content.is_some())
                    .filter_map(|material| material.artifact.clone()),
            )
            .chain(
                turns()
                    .flat_map(|turn| &turn.steps)
                    .flat_map(|step| &step.materials)
                    .filter_map(|material| material.artifact.clone()),
            ),
    )
}

pub(super) fn resume_model_steps(
    state: &mut SessionSnapshot,
) -> Vec<(
    String,
    String,
    String,
    bitrouter_sdk::language_model::types::GenerateResult,
)> {
    let mut outputs = Vec::new();
    let cancelled = state
        .run
        .as_ref()
        .is_some_and(|run| run.cancellation.is_some());
    let steered = state
        .agents
        .keys()
        .filter(|id| steering::has_pending(state, id))
        .cloned()
        .collect::<BTreeSet<_>>();
    for agent in state.agents.values_mut() {
        let Some(turn) = &mut agent.turn else {
            continue;
        };
        let unresolved_tools = turn.invocations.iter().any(|call| {
            call.result
                .as_ref()
                .is_none_or(|result| result.status == ToolOutcome::EffectUnknown)
        });
        if let Some(step) = turn.steps.last_mut().filter(|step| !step.settled) {
            if turn.status.terminal() {
                step.interrupted = true;
                step.settled = true;
                continue;
            }
            // An abandoned driver can have durably marked this model step as
            // requiring recovery. Verified quiescence resolves that blocker;
            // it does not resolve any separate uncertain workspace effect.
            if turn.status == AgentStatus::RecoveryRequired && !unresolved_tools {
                turn.status = if cancelled || turn.cancellation_requested {
                    AgentStatus::Cancelling
                } else {
                    AgentStatus::ModelRunning
                };
            }
            let output = step
                .attempts
                .last()
                .and_then(|attempt| attempt.receipt.as_ref())
                .and_then(|receipt| {
                    receipt
                        .report
                        .result
                        .as_ref()
                        .map(|result| (receipt.report.request_id.clone(), result.clone()))
                });
            let rejected_output = step
                .attempts
                .last()
                .and_then(|attempt| attempt.receipt.as_ref())
                .and_then(|receipt| receipt.report.rejection_reason())
                .or_else(|| auxiliary_output::terminal_reason(step));
            if steered.contains(&agent.agent_id) {
                step.interrupted = true;
                step.settled = true;
                if turn.status != AgentStatus::RecoveryRequired {
                    turn.status = if cancelled || turn.cancellation_requested {
                        AgentStatus::Cancelling
                    } else {
                        AgentStatus::Runnable
                    };
                }
            } else if let Some(reason) = rejected_output {
                // This attempt completed and failed canonical admission. It
                // is not an uncertain interrupted call eligible for retry.
                step.settled = true;
                if cancelled || turn.cancellation_requested {
                    turn.status = AgentStatus::Cancelling;
                } else {
                    turn.status = AgentStatus::Failed;
                    turn.terminal_reason = Some(reason.into());
                }
            } else if let Some((request_id, output)) = output {
                // Replay the ordinary output admission transition, never the
                // provider request. A complete receipt is durable evidence.
                outputs.push((
                    agent.agent_id.clone(),
                    step.step_id.clone(),
                    request_id,
                    output,
                ));
                if turn.status != AgentStatus::RecoveryRequired {
                    turn.status = if cancelled || turn.cancellation_requested {
                        AgentStatus::Cancelling
                    } else {
                        AgentStatus::ModelRunning
                    };
                }
            } else {
                step.interrupted = true;
                step.settled = true;
                if turn.status != AgentStatus::RecoveryRequired {
                    turn.status = if cancelled || turn.cancellation_requested {
                        AgentStatus::Cancelling
                    } else {
                        AgentStatus::Runnable
                    };
                }
            }
        }
    }
    cancel_descendants_of_failed_root(state);
    outputs
}

/// Late evidence can arrive after reconciliation closed the interrupted step.
/// It may fail that same runnable turn, never a new step, run or steered input.
pub(super) fn finish_rejected_output(
    state: &mut SessionSnapshot,
    agent_id: &str,
    agent_turn_id: &str,
    step_id: &str,
) {
    if steering::has_pending(state, agent_id) {
        return;
    }
    let Some(turn) = state
        .agents
        .get_mut(agent_id)
        .and_then(|agent| agent.turn.as_mut())
    else {
        return;
    };
    if turn.agent_turn_id != agent_turn_id || turn.status != AgentStatus::Runnable {
        return;
    }
    if turn.steps.last().is_some_and(|step| {
        step.step_id == step_id
            && step.interrupted
            && step.settled
            // Applied steering changes input before the next step exists.
            // Late accounting for the superseded step cannot fail that input.
            && !state.steering.values().any(|record| {
                record.agent_id == agent_id
                    && record.run_id == turn.run_id
                    && record.agent_turn_id == agent_turn_id
                    && record.disposition == steering::SteeringDisposition::Applied
                    && record
                        .resolved_state_revision
                        .is_some_and(|revision| revision > step.input_state_revision)
            })
            && step
                .attempts
                .last()
                .and_then(|attempt| attempt.receipt.as_ref())
                .is_some_and(|receipt| receipt.report.rejection_reason().is_some())
    }) {
        turn.status = AgentStatus::Failed;
        turn.terminal_reason = turn
            .steps
            .last()
            .and_then(|step| step.attempts.last())
            .and_then(|attempt| attempt.receipt.as_ref())
            .and_then(|receipt| receipt.report.rejection_reason())
            .map(str::to_owned);
        cancel_descendants_of_failed_root(state);
    }
}

fn cancel_descendants_of_failed_root(state: &mut SessionSnapshot) {
    if state
        .agents
        .get(&state.agent_id)
        .and_then(|agent| agent.turn.as_ref())
        .is_some_and(|turn| turn.status == AgentStatus::Failed)
    {
        // Match live root failure: descendants cannot continue detached from
        // a root whose complete output has a durable rejection receipt.
        for (agent_id, agent) in &mut state.agents {
            if agent_id != &state.agent_id {
                agent.queue.clear();
                if let Some(turn) = &mut agent.turn
                    && !turn.status.terminal()
                {
                    turn.status = AgentStatus::Cancelling;
                    turn.cancellation_requested = true;
                }
            }
        }
    }
}
