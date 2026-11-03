use super::*;
use bitrouter_ai::core::checkpoint::{BatchIdentity, Checkpoint, DurableEvent, ToolStartFence};
use bitrouter_ai::core::protocol::VERSION;
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn gate() -> Result<CommitGate, CoreError> {
    CommitGate::new(
        OwnershipGrant {
            session_id: "session".into(),
            harness_id: "harness".into(),
            core_instance_id: "core".into(),
            execution_epoch: 1,
        },
        DurableHead::default(),
        Limits::default(),
    )
}

fn propose(gate: &mut CommitGate, id: &str) -> Result<CheckpointAck, CoreError> {
    let payload = proposal(gate.head(), id);
    let expected_revision = payload.checkpoint.state_revision;
    let expected_seq = gate.head().event_seq + 2;
    let batch = gate.propose(payload)?;
    Ok(CheckpointAck {
        batch_id: id.into(),
        payload_sha256: batch.payload_sha256.clone(),
        state_revision: expected_revision,
        through_event_seq: expected_seq,
        execution_epoch: 1,
    })
}

fn proposal(head: &DurableHead, id: &str) -> CheckpointPayload {
    CheckpointPayload {
        identity: BatchIdentity {
            batch_id: id.into(),
            session_id: "session".into(),
            core_instance_id: "core".into(),
            execution_epoch: 1,
        },
        base_event_seq: head.event_seq,
        base_state_revision: head.state_revision,
        events: (1..=2)
            .map(|offset| DurableEvent {
                event_seq: head.event_seq + offset,
                kind: "session.bound".into(),
                run_id: None,
                agent_id: None,
                payload: json!({"offset": offset}),
            })
            .collect(),
        checkpoint: Checkpoint {
            schema_version: VERSION,
            state_revision: head.state_revision + 1,
            artifact_refs: Vec::new(),
            state: json!({"candidate": id}),
        },
        tool_start_fences: vec![ToolStartFence {
            invocation_id: "tool".into(),
            attempt_id: "attempt".into(),
        }],
    }
}

fn acknowledge(
    gate: &mut CommitGate,
    ack: &CheckpointAck,
    transition: bool,
) -> Result<Option<()>, CoreError> {
    if transition {
        gate.acknowledge_transition(ack)
    } else {
        gate.acknowledge(ack).map(|payload| payload.map(|_| ()))
    }
}

fn reconnect(
    gate: &mut CommitGate,
    head: &DurableHead,
    transition: bool,
) -> Result<Option<()>, CoreError> {
    if transition {
        gate.reconnect_transition(head)
    } else {
        gate.reconnect(head).map(|payload| payload.map(|_| ()))
    }
}

fn mismatched_acks(ack: &CheckpointAck) -> Vec<(CheckpointAck, ErrorCode)> {
    let mut cases = Vec::new();
    let mut wrong = ack.clone();
    wrong.batch_id.push_str("_other");
    cases.push((wrong, ErrorCode::CheckpointConflict));
    let mut wrong = ack.clone();
    wrong.payload_sha256 = if wrong.payload_sha256 == "0".repeat(64) {
        "1".repeat(64)
    } else {
        "0".repeat(64)
    };
    cases.push((wrong, ErrorCode::CheckpointConflict));
    let mut wrong = ack.clone();
    wrong.through_event_seq += 1;
    cases.push((wrong, ErrorCode::CheckpointConflict));
    let mut wrong = ack.clone();
    wrong.state_revision += 1;
    cases.push((wrong, ErrorCode::CheckpointConflict));
    let mut wrong = ack.clone();
    wrong.execution_epoch += 1;
    cases.push((wrong, ErrorCode::StaleEpoch));
    cases
}

#[test]
fn acknowledgements_require_every_field_and_publish_once() -> TestResult {
    for transition in [false, true] {
        let mut gate = gate()?;
        let first = propose(&mut gate, "first")?;
        assert_eq!(acknowledge(&mut gate, &first, transition)?, Some(()));
        assert!(gate.can_dispatch());
        let next = propose(&mut gate, "next")?;
        let pending = gate.pending().cloned().ok_or("pending batch missing")?;
        assert_eq!(acknowledge(&mut gate, &first, transition)?, None);
        assert_eq!(gate.pending(), Some(&pending));
        assert!(!gate.can_dispatch());
        for (wrong, code) in mismatched_acks(&next) {
            assert_eq!(
                acknowledge(&mut gate, &wrong, transition)
                    .err()
                    .map(|error| error.code),
                Some(code)
            );
            assert_eq!(gate.head(), &first.head());
            assert_eq!(gate.pending(), Some(&pending));
            assert!(!gate.can_dispatch());
        }
        assert_eq!(acknowledge(&mut gate, &next, transition)?, Some(()));
        assert_eq!(gate.head(), &next.head());
        assert!(gate.pending().is_none());
        assert!(gate.can_dispatch());
        assert_eq!(acknowledge(&mut gate, &next, transition)?, None);
    }
    Ok(())
}

#[test]
fn reconnect_preserves_retransmission_and_refuses_unrelated_heads() -> TestResult {
    for transition in [false, true] {
        let mut gate = gate()?;
        let first = propose(&mut gate, "first")?;
        acknowledge(&mut gate, &first, transition)?;
        let next = propose(&mut gate, "next")?;
        let pending = gate.pending().cloned().ok_or("pending batch missing")?;
        gate.disconnect();
        assert_eq!(reconnect(&mut gate, &first.head(), transition)?, None);
        assert_eq!(gate.pending(), Some(&pending));
        assert!(!gate.can_dispatch());
        for (wrong, code) in mismatched_acks(&next) {
            assert_eq!(
                reconnect(&mut gate, &wrong.head(), transition)
                    .err()
                    .map(|error| error.code),
                Some(code)
            );
            assert_eq!(gate.head(), &first.head());
            assert_eq!(gate.pending(), Some(&pending));
            assert_eq!(gate.dispatch_fence_for_test(), "Disconnected");
        }
        gate.block_dispatch();
        assert_eq!(reconnect(&mut gate, &next.head(), transition)?, Some(()));
        assert_eq!(gate.head(), &next.head());
        assert!(gate.pending().is_none());
        assert_eq!(gate.dispatch_fence_for_test(), "Provisional");
        assert_eq!(reconnect(&mut gate, &next.head(), transition)?, None);
        assert!(!gate.can_dispatch());
        gate.clear_dispatch_block();
        assert!(gate.can_dispatch());
        gate.release();
        gate.disconnect();
        assert_eq!(reconnect(&mut gate, &next.head(), transition)?, None);
        assert_eq!(gate.dispatch_fence_for_test(), "Released");
        assert!(!gate.can_dispatch());
    }
    Ok(())
}

#[test]
fn public_ack_and_reconnect_return_the_original_complete_payload() -> TestResult {
    let mut gate = gate()?;
    let first = proposal(gate.head(), "first");
    let ack = propose(&mut gate, "first")?;
    assert_eq!(gate.acknowledge(&ack)?, Some(first));
    assert!(gate.acknowledge(&ack)?.is_none());

    let next = proposal(gate.head(), "next");
    let ack = propose(&mut gate, "next")?;
    gate.disconnect();
    assert_eq!(gate.reconnect(&ack.head())?, Some(next));
    assert!(gate.reconnect(&ack.head())?.is_none());
    Ok(())
}
