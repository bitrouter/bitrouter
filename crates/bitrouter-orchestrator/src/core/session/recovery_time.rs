//! Authenticated cumulative activity at a process replacement boundary.
//! Measurements replace the cumulative baseline; they are never added to it.

use super::*;
use crate::core::protocol::{Restore, RunActivityReconciliation};

pub(super) fn reconcile(state: &mut SessionSnapshot, request: &Restore) -> Result<(), CoreError> {
    let Some(run) = &mut state.run else {
        return if request.active_time.is_some() {
            Err(reject(
                ErrorCode::OperationConflict,
                "activity has no root run",
            ))
        } else {
            Ok(())
        };
    };
    let Some(evidence) = &request.active_time else {
        return if run.status.terminal() {
            Ok(())
        } else {
            Err(reject(
                ErrorCode::RecoveryRequired,
                "run activity through the ownership handoff is not reconciled",
            ))
        };
    };
    if evidence.run_id != run.run_id {
        return Err(reject(
            ErrorCode::UnauthorizedScope,
            "activity names another run",
        ));
    }
    if evidence.durable_head != request.binding.durable_head {
        return Err(reject(
            ErrorCode::CheckpointConflict,
            "activity names another durable head",
        ));
    }
    if evidence.active_ms < run.active_ms
        || (run.status.terminal() && evidence.active_ms != run.active_ms)
    {
        return Err(reject(
            ErrorCode::OperationConflict,
            "activity regressed or changed a terminal run",
        ));
    }
    run.active_ms = evidence.active_ms;
    run.activity_reconciliations.push(evidence.clone());
    Ok(())
}

#[derive(Deserialize)]
pub(super) struct ActivityHistory {
    run_id: String,
    active_ms: u64,
    #[serde(default)]
    activity_reconciliations: Vec<RunActivityReconciliation>,
}

pub(super) fn validate_history(
    payload: &CheckpointPayload,
    head: DurableHead,
    prior: &mut Option<(Option<ActivityHistory>, DurableHead)>,
) -> Result<(), CoreError> {
    let current: Option<ActivityHistory> = serde_json::from_value(
        payload
            .checkpoint
            .state
            .get("run")
            .cloned()
            .unwrap_or(Value::Null),
    )
    .map_err(json_error)?;
    let mut new_record = None;
    if let Some(run) = &current {
        let mut revision = 0;
        let mut epoch = 0;
        let mut elapsed = 0;
        for record in &run.activity_reconciliations {
            record.durable_head.validate()?;
            if record.run_id != run.run_id
                || record.durable_head.state_revision <= revision
                || record.durable_head.state_revision >= payload.checkpoint.state_revision
                || record.durable_head.execution_epoch < epoch
                || record.durable_head.execution_epoch > payload.identity.execution_epoch
                || record.active_ms < elapsed
                || record.active_ms > run.active_ms
            {
                return Err(conflict("restored activity history is inconsistent"));
            }
            revision = record.durable_head.state_revision;
            epoch = record.durable_head.execution_epoch;
            elapsed = record.active_ms;
            if revision == payload.base_state_revision {
                new_record = Some(record);
            }
        }
        if let Some((Some(previous), head)) = prior
            && previous.run_id == run.run_id
            && (run.active_ms < previous.active_ms
                || !run
                    .activity_reconciliations
                    .starts_with(&previous.activity_reconciliations)
                || run.activity_reconciliations.len() > previous.activity_reconciliations.len() + 1
                || (run.activity_reconciliations.len() > previous.activity_reconciliations.len()
                    && new_record.is_none_or(|record| &record.durable_head != head)))
        {
            return Err(conflict(
                "restored activity history was erased, rewritten or rebased",
            ));
        }
    }
    let events = payload
        .events
        .iter()
        .filter(|event| event.kind == "session.restored")
        .collect::<Vec<_>>();
    let event_evidence = events
        .first()
        .and_then(|event| event.payload.get("active_time"))
        .filter(|value| !value.is_null());
    if events.len() > 1 || event_evidence != new_record.map(encode).transpose()?.as_ref() {
        return Err(conflict(
            "restore event and cumulative activity evidence disagree",
        ));
    }
    *prior = Some((current, head));
    Ok(())
}

fn conflict(message: &str) -> CoreError {
    reject(ErrorCode::CheckpointConflict, message)
}
