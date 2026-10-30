use super::super::{MAX_RECEIPT_TOTAL, decisions};
use super::*;
use crate::gateway::model::decision::{
    policy::DecisionPolicy,
    types::{
        Answer, DecisionError, DecisionFailure, DecisionRequest, DecisionResponse, DecisionUsage,
        Question,
    },
};
use crate::storage::checkpoint::snapshot::context::view::{CandidateSet, SourceRevision};
use bitrouter_ai::core::{
    checkpoint::{BatchIdentity, Checkpoint, CheckpointPayload},
    protocol::CommitStatus,
};
use serde_json::json;

type Result<T> = anyhow::Result<T>;
const RESPONSE: usize = 2 * 128 * 1024 + 8192;

fn pending(id: &str) -> Result<DecisionReceipt> {
    let request = DecisionRequest {
        model: "judge".into(),
        state: json!("bounded retained evidence"),
        questions: BTreeMap::from([(
            "representation".into(),
            Question::Choice {
                instructions: Some(json!("Choose a representation")),
                criteria: BTreeMap::from([
                    ("full".into(), json!(null)),
                    ("hide".into(), json!(null)),
                ]),
            },
        )]),
    };
    request.validate()?;
    Ok(DecisionReceipt {
        decision_id: id.into(),
        run_id: "run".into(),
        task_id: "task".into(),
        agent_id: "root".into(),
        source: SourceRevision {
            context_revision: 1,
            signal_revision: 1,
            manifest_sha256: "0".repeat(64),
            prompt_sha256: "1".repeat(64),
        },
        candidates: CandidateSet {
            task_id: "task".into(),
            ordered_blocks: Vec::new(),
            required: Default::default(),
            candidates: BTreeMap::new(),
        },
        request_sha256: crate::storage::checkpoint::snapshot::context::digest(&request)?,
        request,
        policy: DecisionPolicy::default(),
        pricing: None,
        intent_state_revision: 1,
        response_limit_bytes: RESPONSE,
        outcome: None,
        elapsed_ms: None,
        stale: false,
        view_ids: Vec::new(),
    })
}

fn answer() -> DecisionResponse {
    DecisionResponse {
        model: "judge".into(),
        usage: DecisionUsage {
            input_tokens: u64::MAX,
            output_tokens: u64::MAX,
        },
        answers: BTreeMap::from([(
            "representation".into(),
            Answer::Choice {
                choice: "full".into(),
                confidence: 1.0,
                probabilities: BTreeMap::from([("full".into(), 1.0), ("hide".into(), 0.0)]),
            },
        )]),
    }
}

fn actual(receipt: &DecisionReceipt) -> Result<usize> {
    Ok(serde_json::to_vec(receipt)?.len())
}

fn payload(receipts: &BTreeMap<String, DecisionReceipt>) -> CheckpointPayload {
    CheckpointPayload {
        identity: BatchIdentity {
            batch_id: "batch".into(),
            session_id: "session".into(),
            core_instance_id: "core".into(),
            execution_epoch: 1,
        },
        base_state_revision: 0,
        base_event_seq: 0,
        events: Vec::new(),
        tool_start_fences: Vec::new(),
        checkpoint: Checkpoint {
            schema_version: 1,
            state_revision: 1,
            artifact_refs: Vec::new(),
            state: json!({"context_store":{"decisions":receipts}}),
        },
    }
}

fn history(bytes: usize) -> Result<BTreeMap<String, DecisionReceipt>> {
    let mut receipts = BTreeMap::new();
    for index in 0..40 {
        let mut receipt = pending(&format!("completed-{index:02}"))?;
        let mut response = answer();
        receipt.outcome = Some(Ok(response.clone()));
        let target = bytes / 40 + usize::from(index < bytes % 40);
        let padding = target
            .checked_sub(actual(&receipt)?)
            .ok_or_else(|| anyhow::anyhow!("history size"))?;
        response.model.push_str(&"x".repeat(padding));
        response.validate(&receipt.request)?;
        receipt.outcome = Some(Ok(response));
        anyhow::ensure!(actual(&receipt)? == target);
        anyhow::ensure!(
            serde_json::to_vec(
                receipt
                    .outcome
                    .as_ref()
                    .ok_or_else(|| anyhow::anyhow!("outcome"))?
            )?
            .len()
                <= RESPONSE
        );
        receipts.insert(receipt.decision_id.clone(), receipt);
    }
    Ok(receipts)
}

#[test]
fn receipt_projection_reserves_every_pending_success_and_stopped_interrupted_completion()
-> Result<()> {
    let first = pending("pending-1")?;
    let second = pending("pending-2")?;
    let mut crowded = history(MAX_RECEIPT_TOTAL - 3 * RESPONSE / 2)?;
    crowded.insert(first.decision_id.clone(), first.clone());
    decisions(&payload(&crowded))?;
    crowded.insert(second.decision_id.clone(), second.clone());
    let current: usize = crowded
        .values()
        .map(actual)
        .collect::<Result<Vec<_>>>()?
        .into_iter()
        .sum();
    anyhow::ensure!(
        current < MAX_RECEIPT_TOTAL,
        "negative must fit current bytes alone"
    );
    let error = match decisions(&payload(&crowded)) {
        Err(error) => error,
        Ok(_) => anyhow::bail!("second pending completion spent already reserved capacity"),
    };
    anyhow::ensure!(
        error.code == ErrorCode::LimitExceeded && error.commit_status == CommitStatus::NotCommitted
    );

    let mut admitted = history(MAX_RECEIPT_TOTAL - 3 * RESPONSE)?;
    admitted.insert(first.decision_id.clone(), first.clone());
    admitted.insert(second.decision_id.clone(), second.clone());
    decisions(&payload(&admitted))?;
    let mut maximum = answer();
    let encoded = serde_json::to_vec(&Ok::<_, DecisionError>(&maximum))?.len();
    maximum.model.push_str(
        &"x".repeat(
            RESPONSE
                .checked_sub(encoded)
                .ok_or_else(|| anyhow::anyhow!("response size"))?,
        ),
    );
    maximum.validate(&first.request)?;
    anyhow::ensure!(serde_json::to_vec(&Ok::<_, DecisionError>(&maximum))?.len() == RESPONSE);
    let mut completed = first;
    completed.outcome = Some(Ok(maximum));
    completed.elapsed_ms = Some(u64::MAX);
    completed.stale = true;
    admitted.insert(completed.decision_id.clone(), completed);
    let mut stopped = second;
    stopped.outcome = Some(Err(DecisionError {
        kind: DecisionFailure::Interrupted,
        message: "previous owner stopped before recording the decision outcome".into(),
        usage: None,
        may_have_run: true,
    }));
    // Stopped-owner reconciliation leaves elapsed unknown. This projection does
    // not attest an owner stopped or authorize a recovery/provider replay.
    anyhow::ensure!(stopped.elapsed_ms.is_none());
    admitted.insert(stopped.decision_id.clone(), stopped);
    let decoded = decisions(&payload(&admitted))?;
    anyhow::ensure!(decoded.values().all(|receipt| receipt.outcome.is_some()));
    Ok(())
}

#[test]
fn pending_metadata_allowance_covers_all_error_shapes_elapsed_and_stale_without_cap_changes()
-> Result<()> {
    let pending = pending("errors")?;
    let reserved = receipt(&pending, actual(&pending)?)?;
    for kind in [
        DecisionFailure::InvalidRequest,
        DecisionFailure::Transport,
        DecisionFailure::Http,
        DecisionFailure::InvalidResponse,
        DecisionFailure::ResponseTooLarge,
        DecisionFailure::Timeout,
        DecisionFailure::Cancelled,
        DecisionFailure::Interrupted,
    ] {
        let mut terminal = pending.clone();
        terminal.outcome = Some(Err(DecisionError {
            kind,
            message: "\"\\\n".repeat(40),
            usage: Some(DecisionUsage {
                input_tokens: u64::MAX,
                output_tokens: u64::MAX,
            }),
            may_have_run: true,
        }));
        terminal.elapsed_ms = Some(u64::MAX);
        terminal.stale = true;
        anyhow::ensure!(actual(&terminal)? <= reserved && reserved <= MAX_RECEIPT);
        let actual = actual(&terminal)?;
        anyhow::ensure!(receipt(&terminal, actual)? == actual);
    }
    let mut impossible = pending;
    impossible.response_limit_bytes = MAX_RECEIPT;
    anyhow::ensure!(
        matches!(receipt(&impossible, actual(&impossible)?), Err(error) if error.code == ErrorCode::LimitExceeded)
    );
    Ok(())
}

#[test]
fn link_map_reserves_all_pending_outcome_digests_before_acceptance() -> Result<()> {
    use bitrouter_ai::tenancy::{
        authority::{OwnerKind, ResourceKind},
        configuration::ResourceRef,
    };
    let reference = ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "t".repeat(128).try_into()?,
        resource_kind: ResourceKind::Route,
        resource_id: "r".repeat(128).try_into()?,
        revision_id: "v".repeat(128).try_into()?,
    };
    let prototype = Record {
        seeds: None,
        terminal_digests: None,
        version: 1,
        installation: "i".repeat(128).try_into()?,
        tenant: "t".repeat(128).try_into()?,
        revision: "v".repeat(128).try_into()?,
        parent: reference.clone(),
        child: ResourceRef {
            resource_id: "c".repeat(128).try_into()?,
            ..reference
        },
        parent_execution: "p".repeat(128).try_into()?,
        child_execution: "c".repeat(128).try_into()?,
        profile_digest: "0".repeat(64),
        native_digest: "1".repeat(64),
        decision_id: "d".repeat(128),
        step_id: "s".repeat(256),
        epoch: 1,
        intent_digest: "2".repeat(64),
        outcome_digest: None,
    };
    let mut entries = BTreeMap::new();
    // Build one exact current-size map, without repeatedly serializing its prefix.
    // The identities respect their real bounded types; this test supplies no grant.
    let one = serde_json::to_vec(&prototype)?.len()
        + serde_json::to_vec(&prototype.decision_id)?.len()
        + 2;
    let count = (MAX_LINK_TOTAL - 1) / (one + OUTCOME_DIGEST);
    anyhow::ensure!(count + 1 < super::super::MAX_LINKS && count > 2);
    for index in 0..=count {
        let id = format!("decision-{index:04}-{}", "d".repeat(114));
        anyhow::ensure!(id.len() == 128);
        let record = Record {
            decision_id: id.clone(),
            ..prototype.clone()
        };
        entries.insert(id, record);
    }
    anyhow::ensure!(serde_json::to_vec(&entries)?.len() < MAX_LINK_TOTAL);
    anyhow::ensure!(
        matches!(links(&entries), Err(error) if error.code == ErrorCode::LimitExceeded)
    );
    entries.pop_last();
    links(&entries)?;
    for record in entries.values_mut() {
        record.outcome_digest = Some("3".repeat(64));
    }
    // The terminal map itself, including quoted keys and all real digest strings,
    // fits without shrinking or deleting any admitted entry.
    anyhow::ensure!(serde_json::to_vec(&entries)?.len() <= MAX_LINK_TOTAL);
    anyhow::ensure!(entries.values().all(|entry| serde_json::to_vec(entry).is_ok_and(|bytes| bytes.len() <= MAX_LINK_BYTES)));
    links(&entries)?;
    Ok(())
}
