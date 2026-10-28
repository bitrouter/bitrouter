//! Authenticated cumulative activity at a process replacement boundary.
//! Measurements replace the cumulative baseline; they are never added to it.

use super::*;
use crate::storage::checkpoint::snapshot::ActivityGap;
use bitrouter_ai::core::protocol::{Restore, RunActivityReconciliation};

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
    if state.activity_gaps.contains_key(&run.run_id)
        && (!run.status.terminal() || request.active_time.is_some())
    {
        return Err(reject(
            ErrorCode::RecoveryRequired,
            "retired activity uncertainty cannot authorize a run or a clock handoff",
        ));
    }
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

/// The sealed stopped-owner participant authorizes only cancellation. Retain
/// the missing interval without fabricating an ordinary clock measurement.
#[cfg(feature = "harness")]
pub(super) fn retire(
    state: &mut SessionSnapshot,
    request: &Restore,
    checked: &crate::tenancy::session::recovery::CheckedRecovery,
) -> Result<Option<ActivityGap>, CoreError> {
    checked.validate_snapshot(state).map_err(|_| {
        reject(
            ErrorCode::RecoveryRequired,
            "native snapshot is outside the stopped recovery profile",
        )
    })?;
    if request.active_time.is_some() || state.run.as_ref().is_none_or(|run| run.status.terminal()) {
        reconcile(state, request)?;
        return Ok(None);
    }
    let run = state
        .run
        .as_ref()
        .ok_or_else(|| conflict("native retirement has no run"))?;
    if state.activity_gaps.contains_key(&run.run_id) {
        // A crash after the first repair cannot erase or rebase its uncertainty.
        return Ok(None);
    }
    let gap = ActivityGap {
        durable_head: request.binding.durable_head.clone(),
        known_active_ms: run.active_ms,
    };
    state.activity_gaps.insert(run.run_id.clone(), gap.clone());
    Ok(Some(gap))
}

pub(super) struct ActivityHistory {
    run: Option<RunHistory>,
    gaps: BTreeMap<String, ActivityGap>,
}

#[derive(Deserialize)]
struct RunHistory {
    run_id: String,
    active_ms: u64,
    #[serde(default)]
    activity_reconciliations: Vec<RunActivityReconciliation>,
}

pub(super) fn validate_history(
    payload: &CheckpointPayload,
    head: DurableHead,
    prior: &mut Option<(ActivityHistory, DurableHead)>,
) -> Result<(), CoreError> {
    let current = ActivityHistory {
        run: serde_json::from_value(
            payload
                .checkpoint
                .state
                .get("run")
                .cloned()
                .unwrap_or(Value::Null),
        )
        .map_err(json_error)?,
        gaps: serde_json::from_value(
            payload
                .checkpoint
                .state
                .get("activity_gaps")
                .cloned()
                .unwrap_or_else(|| json!({})),
        )
        .map_err(json_error)?,
    };
    let mut new_record = None;
    let mut new_gap = None;
    for (run_id, gap) in &current.gaps {
        validate_id(run_id)?;
        let source = &gap.durable_head;
        source.validate()?;
        if source.state_revision == 0
            || source.state_revision >= payload.checkpoint.state_revision
            || source.execution_epoch > payload.identity.execution_epoch
            || source.event_seq > payload.base_event_seq
        {
            return Err(conflict("retired activity gap has an invalid source head"));
        }
        if source.state_revision == payload.base_state_revision {
            if new_gap.is_some()
                || current
                    .run
                    .as_ref()
                    .is_none_or(|run| &run.run_id != run_id || run.active_ms != gap.known_active_ms)
            {
                return Err(conflict(
                    "new activity gap is not the current run's known boundary",
                ));
            }
            new_gap = Some((run_id, gap));
        }
    }
    if let Some((previous, head)) = prior
        && (previous
            .gaps
            .iter()
            .any(|(run_id, gap)| current.gaps.get(run_id) != Some(gap))
            || current.gaps.iter().any(|(run_id, gap)| {
                !previous.gaps.contains_key(run_id)
                    && (new_gap != Some((run_id, gap)) || &gap.durable_head != head)
            }))
    {
        return Err(conflict(
            "retired activity gap was erased, rewritten or rebased",
        ));
    }
    if let Some(run) = &current.run {
        let gap = current.gaps.get(&run.run_id);
        if gap.is_some_and(|gap| gap.known_active_ms > run.active_ms) {
            return Err(conflict("retired activity lower bound regressed"));
        }
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
                || gap.is_some_and(|gap| {
                    record.durable_head.state_revision >= gap.durable_head.state_revision
                })
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
        if let Some((history, head)) = prior
            && let Some(previous) = &history.run
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
    let event_gap = events
        .first()
        .and_then(|event| event.payload.get("activity_gap"))
        .filter(|value| !value.is_null());
    if event_gap != new_gap.map(|(_, gap)| encode(gap)).transpose()?.as_ref()
        || (event_gap.is_some() && event_evidence.is_some())
        || new_gap.is_some_and(|(run_id, _)| {
            events
                .first()
                .is_none_or(|event| event.run_id.as_ref() != Some(run_id))
        })
    {
        return Err(conflict("restore event and retired activity gap disagree"));
    }
    *prior = Some((current, head));
    Ok(())
}

fn conflict(message: &str) -> CoreError {
    reject(ErrorCode::CheckpointConflict, message)
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::core::checkpoint::{BatchIdentity, Checkpoint, DurableEvent};

    fn head(revision: u64) -> DurableHead {
        DurableHead {
            execution_epoch: 1,
            state_revision: revision,
            event_seq: revision,
            batch_id: Some(format!("batch-{revision}")),
            payload_sha256: Some("0".repeat(64)),
        }
    }

    fn payload(revision: u64, gap: Option<DurableHead>) -> CheckpointPayload {
        let gap = gap.map(|durable_head| ActivityGap {
            durable_head,
            known_active_ms: 25,
        });
        let gaps: BTreeMap<_, _> = gap
            .clone()
            .map(|gap| ("run".to_owned(), gap))
            .into_iter()
            .collect();
        let mut events = Vec::new();
        if revision == 2 {
            events.push(DurableEvent {
                event_seq: 2,
                kind: "session.restored".into(),
                run_id: Some("run".into()),
                agent_id: None,
                payload: json!({"active_time":null,"activity_gap":gap}),
            });
        }
        CheckpointPayload {
            identity: BatchIdentity {
                batch_id: format!("batch-{revision}"),
                session_id: "session".into(),
                execution_epoch: 1,
                core_instance_id: "core".into(),
            },
            base_event_seq: revision - 1,
            base_state_revision: revision - 1,
            events,
            tool_start_fences: Vec::new(),
            checkpoint: Checkpoint {
                schema_version: 1,
                state_revision: revision,
                artifact_refs: Vec::new(),
                state: json!({"run":{"run_id":"run","active_ms":25,
                    "activity_reconciliations":[]},"activity_gaps":gaps}),
            },
        }
    }

    #[test]
    fn retirement_gap_survives_later_history_and_rejects_erasure_or_rebasing()
    -> Result<(), CoreError> {
        let first = payload(1, None);
        let retired = payload(2, Some(head(1)));
        let later = payload(3, Some(head(1)));
        let mut history = None;
        validate_history(&first, head(1), &mut history)?;
        validate_history(&retired, head(2), &mut history)?;
        validate_history(&later, head(3), &mut history)?;
        let mut next_run = payload(4, Some(head(1)));
        next_run.checkpoint.state["run"]["run_id"] = json!("next-run");
        next_run.checkpoint.state["run"]["active_ms"] = json!(0);
        validate_history(&next_run, head(4), &mut history)?;

        for change in [
            "erase",
            "erase_after_new_run",
            "rewrite",
            "rebase",
            "bound",
            "measure",
            "regress",
        ] {
            let mut history = None;
            validate_history(&first, head(1), &mut history)?;
            validate_history(&retired, head(2), &mut history)?;
            let mut changed = later.clone();
            match change {
                "erase" => changed.checkpoint.state["activity_gaps"] = json!({}),
                "erase_after_new_run" => {
                    changed.checkpoint.state["activity_gaps"] = json!({});
                    changed.checkpoint.state["run"]["run_id"] = json!("next-run");
                    changed.checkpoint.state["run"]["active_ms"] = json!(0);
                }
                "bound" => {
                    changed.checkpoint.state["activity_gaps"]["run"]["known_active_ms"] = json!(24)
                }
                "rewrite" => {
                    changed.checkpoint.state["activity_gaps"]["run"]["durable_head"]["payload_sha256"] =
                        json!("1".repeat(64))
                }
                "rebase" => {
                    changed.checkpoint.state["activity_gaps"]["run"]["durable_head"] =
                        json!(head(2))
                }
                "measure" => {
                    changed.checkpoint.state["run"]["activity_reconciliations"] = json!([
                        {"run_id":"run","durable_head":head(2),"active_ms":25}
                    ])
                }
                _ => changed.checkpoint.state["run"]["active_ms"] = json!(24),
            }
            let error = validate_history(&changed, head(3), &mut history)
                .err()
                .ok_or_else(|| conflict("changed activity gap was accepted"))?;
            assert_eq!(error.code, ErrorCode::CheckpointConflict, "{change}");
        }
        Ok(())
    }

    #[test]
    fn retirement_gap_requires_its_exact_source_head_and_restore_event() -> Result<(), CoreError> {
        for change in ["event", "digest", "future", "clock"] {
            let mut history = None;
            validate_history(&payload(1, None), head(1), &mut history)?;
            let mut changed = payload(2, Some(head(1)));
            match change {
                "event" => changed.events[0].payload["activity_gap"] = Value::Null,
                "digest" => {
                    changed.checkpoint.state["activity_gaps"]["run"]["durable_head"]["payload_sha256"] =
                        json!("1".repeat(64));
                    changed.events[0].payload["activity_gap"] =
                        changed.checkpoint.state["activity_gaps"]["run"].clone();
                }
                "future" => {
                    changed.checkpoint.state["activity_gaps"]["run"]["durable_head"] =
                        json!(head(2));
                    changed.events[0].payload["activity_gap"] =
                        changed.checkpoint.state["activity_gaps"]["run"].clone();
                }
                _ => {
                    changed.events[0].payload["active_time"] =
                        json!({"run_id":"run","durable_head":head(1),"active_ms":25})
                }
            }
            let error = validate_history(&changed, head(2), &mut history)
                .err()
                .ok_or_else(|| conflict("unanchored activity gap was accepted"))?;
            assert_eq!(error.code, ErrorCode::CheckpointConflict, "{change}");
        }
        Ok(())
    }
}
