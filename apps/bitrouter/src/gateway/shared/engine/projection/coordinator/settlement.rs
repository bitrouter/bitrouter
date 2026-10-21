//! Shared settlement uses original facts; no current pricing or synthetic timing.

use bitrouter_ai::gateway::execution::{ExecutionState, TerminalOutcome};
use chrono::{DateTime, SecondsFormat};

use super::*;
use crate::trajectory::types::{
    RequestStatus, Settlement, StoredRequest, TrajectoryEvent, TrajectoryEventKind,
};

pub(super) fn build(
    terminal: &Terminal,
    request: &StoredRequest,
    events: &[TrajectoryEvent],
    sequence: u64,
) -> Result<Settlement> {
    let binding = terminal
        .binding
        .as_ref()
        .context("missing trajectory binding")?;
    ensure!(
        request.request_id == binding.request_id
            && request.episode_id == binding.episode_id
            && request.full_input_digest.key_id() == binding.correlation_epoch,
        "trajectory projection changed original binding"
    );
    let route = events
        .iter()
        .find(|event| {
            event.request_id.as_deref() == Some(&binding.request_id)
                && event.kind == TrajectoryEventKind::RouteIntentRecorded
        })
        .context("trajectory settlement has no original route intent")?;
    let decision = crate::trajectory::evaluation::decode_eval_decision(route)?
        .context("trajectory route has no original replayable decision")?;
    terminal.inputs.check_trajectory_decision(&decision)?;
    let status = match &terminal.record.event.execution.state {
        ExecutionState::Terminal { result } if result.outcome == TerminalOutcome::Succeeded => {
            RequestStatus::Settled
        }
        ExecutionState::Terminal { .. } | ExecutionState::Resolved { .. } => RequestStatus::Failed,
        _ => anyhow::bail!("trajectory projection requires final execution evidence"),
    };
    let durable_at = DateTime::from_timestamp_millis(terminal.record.event.execution.updated_at_ms)
        .context("terminal timestamp exceeds supported range")?;
    let head = DateTime::parse_from_rfc3339(
        &events
            .last()
            .context("trajectory history is missing")?
            .captured_at,
    )?
    .to_utc();
    let captured_at = std::cmp::max(durable_at, head).to_rfc3339_opts(SecondsFormat::AutoSi, true);
    let mut evidence = terminal.inputs.trajectory_evidence(&terminal.record)?;
    if matches!(
        terminal.record.event.execution.state,
        ExecutionState::Resolved { .. }
    ) {
        evidence
            .categorical
            .insert("settlement.outcome".into(), "failed".into());
        evidence.categorical.insert(
            "settlement.resolution".into(),
            "operator_acknowledged".into(),
        );
    }
    crate::trajectory::settlement::build_from_evidence(
        &binding.owner_user_id,
        request,
        events,
        sequence,
        evidence,
        captured_at,
        status,
    )
}

pub(super) fn reference(settlement: &Settlement) -> Result<TrajectoryReference> {
    let outbox = settlement
        .outbox
        .as_ref()
        .context("required trajectory evaluation outbox is missing")?;
    let envelope = outbox
        .payload
        .evaluation
        .as_ref()
        .context("required trajectory evaluation envelope is missing")?;
    Ok(TrajectoryReference {
        settlement_id: settlement.event.event_id.clone(),
        settlement_digest: settlement.event.content_digest.clone(),
        outbox_id: outbox.outbox_id.clone(),
        outbox_digest: crate::trajectory::types::canonical_digest(&outbox.payload)?,
        subject_id: envelope.subject.eval_id.clone(),
        subject_digest: subject_digest(&envelope.subject)?,
        result_digest: crate::storage::evaluation::result_digest(&envelope.result)?,
    })
}
