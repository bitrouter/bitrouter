mod support;

use std::collections::BTreeMap;

use base64::Engine;
use base64::engine::general_purpose::STANDARD;
use bitrouter_orchestrator::core::checkpoint::{
    BatchIdentity, Checkpoint, CheckpointBatch, CheckpointPayload, CommitGate, DurableEvent,
    DurableHead, ToolStartFence, sha256,
};
use bitrouter_orchestrator::core::protocol::{
    ArtifactRef, Capabilities, ClientMessage, Command, CoreError, ErrorCode, HarnessManifest,
    HarnessTool, Limits, OwnershipGrant, RoutingSettings, ToolEffect, VERSION,
};
use serde_json::json;
use support::DurableHarness;

type TestResult = Result<(), Box<dyn std::error::Error>>;

#[test]
fn tool_start_fences_commit_atomically_and_survive_restart_and_epoch_change() -> TestResult {
    for start_first in [false, true] {
        let mut harness = DurableHarness::new(grant());
        let identity = ToolStartFence {
            invocation_id: "tool_1".into(),
            attempt_id: "attempt_1".into(),
        };
        if start_first {
            assert!(harness.try_start_tool(identity.clone()));
        }
        let mut payload = proposal(&harness.head, "fence");
        payload.tool_start_fences.push(identity.clone());
        let batch = CheckpointBatch::encode(&payload, &harness.limits)?;
        harness.fail_next_commit = true;
        assert!(harness.commit(&batch).is_err());
        assert!(harness.tool_start_fences.is_empty());
        assert_eq!(harness.head, DurableHead::default());
        let ack = harness.commit(&batch)?;
        assert!(harness.tool_start_fences.contains(&identity));
        assert_eq!(harness.commit(&batch)?, ack);
        assert!(!harness.try_start_tool(identity.clone()));
        assert_eq!(harness.started_tools.contains(&identity), start_first);
        let mut restored = harness.clone();
        restored.grant.execution_epoch += 1;
        assert!(!restored.try_start_tool(identity.clone()));
        assert_eq!(restored.started_tools.contains(&identity), start_first);
    }
    Ok(())
}

#[test]
fn tool_start_fences_reject_invalid_and_duplicate_identities() -> TestResult {
    let mut payload = proposal(&DurableHead::default(), "fence");
    payload.tool_start_fences.push(ToolStartFence {
        invocation_id: "tool_1".into(),
        attempt_id: "attempt_1".into(),
    });
    payload.tool_start_fences.push(ToolStartFence {
        invocation_id: "tool_1".into(),
        attempt_id: "attempt_2".into(),
    });
    assert!(CheckpointBatch::encode(&payload, &Limits::default()).is_err());
    payload.tool_start_fences.pop();
    payload.tool_start_fences[0].attempt_id.clear();
    assert!(CheckpointBatch::encode(&payload, &Limits::default()).is_err());
    Ok(())
}

fn grant() -> OwnershipGrant {
    OwnershipGrant {
        session_id: "session_1".into(),
        harness_id: "harness_1".into(),
        core_instance_id: "core_1".into(),
        execution_epoch: 1,
    }
}

fn proposal(base: &DurableHead, batch: &str) -> CheckpointPayload {
    CheckpointPayload {
        identity: BatchIdentity {
            batch_id: batch.into(),
            session_id: "session_1".into(),
            execution_epoch: 1,
            core_instance_id: "core_1".into(),
        },
        base_event_seq: base.event_seq,
        base_state_revision: base.state_revision,
        tool_start_fences: Vec::new(),
        events: vec![DurableEvent {
            event_seq: base.event_seq + 1,
            kind: "session.bound".into(),
            run_id: None,
            agent_id: None,
            payload: json!({"operation_id":"bind_1"}),
        }],
        checkpoint: Checkpoint {
            schema_version: VERSION,
            state_revision: base.state_revision + 1,
            artifact_refs: Vec::new(),
            state: json!({"operations":{"bind_1":{"disposition":"applied"}}}),
        },
    }
}

fn caps() -> Capabilities {
    Capabilities {
        version: VERSION,
        core_instance_id: "core_1".into(),
        operations: vec!["session.bind".into(), "checkpoint.ack".into()],
        transports: vec!["in_process".into()],
        unsupported_features: vec![
            "response.inject".into(),
            "openai_encrypted_agent_message".into(),
        ],
        limits: Limits::default(),
        max_sessions: 16,
        max_host_model_attempts: 16,
    }
}

fn manifest() -> Result<HarnessManifest, CoreError> {
    let tools = vec![HarnessTool {
        name: "read".into(),
        description: "Read workspace text".into(),
        parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
        effect: ToolEffect::Read,
        approval_required: false,
    }];
    Ok(HarnessManifest {
        tool_manifest_digest: HarnessManifest::digest(&tools)?,
        tools,
        workspace_id: "workspace_1".into(),
        workspace_revision: None,
        permission_revision: 1,
        max_tool_output_bytes: 8192,
        artifact_quota_bytes: 1024 * 1024,
        max_artifact_chunk_bytes: 8192,
        required_features: vec!["session.bind".into()],
    })
}

#[test]
fn no_dispatch_before_matching_atomic_ack() -> TestResult {
    let mut gate = CommitGate::new(grant(), DurableHead::default(), Limits::default())?;
    let mut harness = DurableHarness::new(grant());
    assert!(!gate.can_dispatch());
    let batch = gate
        .propose(proposal(&DurableHead::default(), "batch_1"))?
        .clone();
    assert!(!gate.can_dispatch());
    assert_eq!(
        gate.propose(proposal(&DurableHead::default(), "batch_2"))
            .err()
            .map(|e| e.code),
        Some(ErrorCode::Busy)
    );
    let ack = harness.commit(&batch)?;
    assert!(!gate.can_dispatch());
    let mut wrong = ack.clone();
    wrong.through_event_seq += 1;
    assert!(gate.acknowledge(&wrong).is_err());
    assert_eq!(gate.head().state_revision, 0);
    assert!(gate.acknowledge(&ack)?.is_some());
    assert!(gate.can_dispatch());
    assert!(gate.acknowledge(&ack)?.is_none());
    assert_eq!(harness.commit(&batch)?, ack);
    assert_eq!(harness.batches.len(), 1);
    Ok(())
}

#[test]
fn failed_commit_preserves_tentative_batch_and_durable_head() -> TestResult {
    let mut gate = CommitGate::new(grant(), DurableHead::default(), Limits::default())?;
    let mut harness = DurableHarness::new(grant());
    harness.fail_next_commit = true;
    let batch = gate.propose(proposal(&harness.head, "batch_1"))?.clone();
    assert!(harness.commit(&batch).is_err());
    assert!(!gate.can_dispatch());
    assert_eq!(harness.head, DurableHead::default());
    assert!(harness.batches.is_empty());
    assert_eq!(gate.pending(), Some(&batch));
    gate.acknowledge(&harness.commit(&batch)?)?;
    assert!(gate.can_dispatch());
    Ok(())
}

#[test]
fn ack_loss_reconnect_adopts_head_without_reappending() -> TestResult {
    let mut harness = DurableHarness::new(grant());
    let mut gate = CommitGate::new(grant(), harness.head.clone(), Limits::default())?;
    let batch = gate.propose(proposal(&harness.head, "batch_1"))?.clone();
    harness.commit(&batch)?;
    gate.disconnect();
    assert!(!gate.can_dispatch());
    let restarted_harness = harness.clone();
    assert!(gate.reconnect(&restarted_harness.head)?.is_some());
    assert!(gate.can_dispatch());
    assert!(gate.reconnect(&restarted_harness.head)?.is_none());
    assert_eq!(restarted_harness.batches.len(), 1);
    Ok(())
}

#[test]
fn disconnect_and_provisional_cancel_remain_independent_barriers() -> TestResult {
    let mut harness = DurableHarness::new(grant());
    let mut gate = CommitGate::new(grant(), harness.head.clone(), Limits::default())?;
    let batch = gate.propose(proposal(&harness.head, "batch_1"))?.clone();
    gate.block_dispatch();
    gate.acknowledge(&harness.commit(&batch)?)?;
    assert!(!gate.can_dispatch());
    gate.disconnect();
    gate.clear_dispatch_block();
    assert!(!gate.can_dispatch());
    assert!(gate.propose(proposal(&harness.head, "batch_2")).is_err());
    gate.reconnect(&harness.head)?;
    assert!(gate.can_dispatch());
    gate.release();
    gate.reconnect(&harness.head)?;
    assert!(!gate.can_dispatch());
    Ok(())
}

#[test]
fn exact_bytes_are_hashed_and_envelope_cannot_be_relabelled() -> TestResult {
    let limits = Limits::default();
    let source = proposal(&DurableHead::default(), "batch_1");
    let batch = CheckpointBatch::encode(&source, &limits)?;
    assert_eq!(batch.decode(&limits)?, source);
    let mut corrupt = batch.clone();
    corrupt.identity.session_id = "another_session".into();
    assert!(corrupt.decode(&limits).is_err());
    let mut changed = batch.clone();
    let mut bytes = STANDARD.decode(&batch.payload_bytes)?;
    bytes.push(b' ');
    changed.payload_bytes = STANDARD.encode(&bytes);
    assert!(changed.decode(&limits).is_err());
    changed.payload_sha256 = sha256(&bytes);
    assert_eq!(changed.decode(&limits)?, source);
    assert_ne!(batch.payload_sha256, changed.payload_sha256);
    Ok(())
}

#[test]
fn altered_retry_and_wrong_append_base_do_not_mutate_store() -> TestResult {
    let mut harness = DurableHarness::new(grant());
    let original = proposal(&harness.head, "batch_1");
    let batch = CheckpointBatch::encode(&original, &harness.limits)?;
    harness.commit(&batch)?;
    let before = harness.head.clone();
    let mut altered = original.clone();
    altered.checkpoint.state = json!({"other":true});
    assert!(
        harness
            .commit(&CheckpointBatch::encode(&altered, &harness.limits)?)
            .is_err()
    );
    let stale = CheckpointBatch::encode(
        &proposal(&DurableHead::default(), "batch_2"),
        &harness.limits,
    )?;
    assert!(harness.commit(&stale).is_err());
    assert_eq!(harness.head, before);
    assert_eq!(harness.batches.len(), 1);
    Ok(())
}

#[test]
fn old_epoch_is_fenced_even_for_exact_batch_retry() -> TestResult {
    let mut harness = DurableHarness::new(grant());
    let batch = CheckpointBatch::encode(&proposal(&harness.head, "batch_1"), &harness.limits)?;
    harness.commit(&batch)?;
    harness.grant.execution_epoch = 2;
    harness.grant.core_instance_id = "core_2".into();
    assert!(harness.commit(&batch).is_err());
    let mut takeover = CommitGate::new(
        harness.grant.clone(),
        harness.head.clone(),
        harness.limits.clone(),
    )?;
    assert!(!takeover.can_dispatch());
    let mut next = proposal(&harness.head, "batch_2");
    next.identity.execution_epoch = 2;
    next.identity.core_instance_id = "core_2".into();
    let batch = takeover.propose(next)?.clone();
    takeover.acknowledge(&harness.commit(&batch)?)?;
    assert!(takeover.can_dispatch());
    Ok(())
}

#[test]
fn artifacts_must_be_complete_and_match_all_reference_fields() -> TestResult {
    let mut harness = DurableHarness::new(grant());
    let data = b"required instructions";
    let reference = ArtifactRef {
        artifact_id: "artifact_1".into(),
        sha256: sha256(data),
        bytes: data.len() as u64,
        media_type: "text/plain".into(),
    };
    let mut payload = proposal(&harness.head, "batch_1");
    payload.checkpoint.artifact_refs.push(reference.clone());
    let batch = CheckpointBatch::encode(&payload, &harness.limits)?;
    assert_eq!(
        harness.commit(&batch).err().map(|e| e.code),
        Some(ErrorCode::ArtifactUnavailable)
    );
    assert!(harness.put_artifact(reference.clone(), &data[..2]).is_err());
    harness.put_artifact(reference, data)?;
    harness.commit(&batch)?;
    let mut next = proposal(&harness.head, "batch_2");
    let mut wrong_length = payload.checkpoint.artifact_refs[0].clone();
    wrong_length.bytes += 1;
    next.checkpoint.artifact_refs.push(wrong_length);
    assert!(
        harness
            .commit(&CheckpointBatch::encode(&next, &harness.limits)?)
            .is_err()
    );
    Ok(())
}

#[test]
fn malformed_sequence_schema_and_size_are_rejected() -> TestResult {
    let limits = Limits::default();
    let mut payload = proposal(&DurableHead::default(), "batch_1");
    payload.events[0].event_seq = 3;
    assert!(CheckpointBatch::encode(&payload, &limits).is_err());
    payload.events[0].event_seq = 1;
    payload.checkpoint.schema_version = 2;
    assert_eq!(
        CheckpointBatch::encode(&payload, &limits)
            .err()
            .map(|e| e.code),
        Some(ErrorCode::UnsupportedVersion)
    );
    payload.checkpoint.schema_version = 1;
    let small = Limits {
        checkpoint_bytes: 16,
        ..limits.clone()
    };
    assert_eq!(
        CheckpointBatch::encode(&payload, &small)
            .err()
            .map(|e| e.code),
        Some(ErrorCode::LimitExceeded)
    );
    let batch = CheckpointBatch::encode(&payload, &limits)?;
    assert!(batch.decode(&small).is_err());
    Ok(())
}

#[test]
fn capabilities_reject_unsupported_requirements_and_escalated_limits() -> TestResult {
    let capabilities = caps();
    capabilities.negotiate(1, &["session.bind".into()], &Limits::default())?;
    assert_eq!(
        capabilities
            .negotiate(2, &[], &Limits::default())
            .err()
            .map(|e| e.code),
        Some(ErrorCode::UnsupportedVersion)
    );
    assert_eq!(
        capabilities
            .negotiate(1, &["response.inject".into()], &Limits::default())
            .err()
            .map(|e| e.code),
        Some(ErrorCode::UnsupportedCapability)
    );
    let raised = Limits {
        active_models: 5,
        ..Limits::default()
    };
    assert_eq!(
        capabilities
            .negotiate(1, &[], &raised)
            .err()
            .map(|e| e.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert!(
        Limits {
            active_models: 0,
            ..Limits::default()
        }
        .validate()
        .is_err()
    );
    Ok(())
}

#[test]
fn manifest_reserves_core_tools_and_verifies_digest() -> TestResult {
    let mut manifest = manifest()?;
    manifest.validate(&caps(), &Limits::default())?;
    manifest.tools[0].name = "spawn_agent".into();
    assert!(manifest.validate(&caps(), &Limits::default()).is_err());
    manifest.tool_manifest_digest = HarnessManifest::digest(&manifest.tools)?;
    assert_eq!(
        manifest
            .validate(&caps(), &Limits::default())
            .err()
            .map(|e| e.code),
        Some(ErrorCode::UnsupportedCapability)
    );
    Ok(())
}

#[test]
fn wire_controls_roundtrip_and_require_scope_epoch_revision() -> TestResult {
    let wire = json!({"version":1,"session_id":"session_1","execution_epoch":1,"operation_id":"cancel_1","expected_state_revision":3,"type":"run.cancel","payload":{"run_id":"run_1"}});
    let mut message: ClientMessage = serde_json::from_value(wire.clone())?;
    assert_eq!(serde_json::to_value(&message)?, wire);
    message.validate(&grant(), &Limits::default())?;
    message.expected_state_revision = None;
    assert_eq!(
        message
            .validate(&grant(), &Limits::default())
            .err()
            .map(|e| e.code),
        Some(ErrorCode::StaleRevision)
    );
    message.expected_state_revision = Some(3);
    message.execution_epoch = 2;
    assert_eq!(
        message
            .validate(&grant(), &Limits::default())
            .err()
            .map(|e| e.code),
        Some(ErrorCode::StaleEpoch)
    );
    message.execution_epoch = 1;
    message.session_id = "other".into();
    assert_eq!(
        message
            .validate(&grant(), &Limits::default())
            .err()
            .map(|e| e.code),
        Some(ErrorCode::UnauthorizedScope)
    );
    assert!(serde_json::from_value::<ClientMessage>(json!({"version":1,"session_id":"session_1","execution_epoch":1,"operation_id":"a","type":"response.inject","payload":{}})).is_err());
    assert_eq!(
        serde_json::from_value::<RoutingSettings>(json!({}))?,
        RoutingSettings::default()
    );
    Ok(())
}

#[test]
fn tool_result_does_not_require_unrelated_scheduling_revision() -> TestResult {
    let wire = json!({"version":1,"session_id":"session_1","execution_epoch":1,"operation_id":"result_1","type":"tool.result","payload":{"invocation_id":"invocation_1","attempt_id":"attempt_1","status":"succeeded","output":"ok","evidence":[],"workspace_revision":null}});
    let message: ClientMessage = serde_json::from_value(wire.clone())?;
    assert!(matches!(message.command, Command::ToolResult(_)));
    message.validate(&grant(), &Limits::default())?;
    assert_eq!(serde_json::to_value(&message)?, wire);
    Ok(())
}

#[test]
fn cannot_repropose_committed_batch_or_adopt_unrelated_head() -> TestResult {
    let mut harness = DurableHarness::new(grant());
    let mut gate = CommitGate::new(grant(), harness.head.clone(), Limits::default())?;
    let original = proposal(&harness.head, "batch_1");
    let batch = gate.propose(original.clone())?.clone();
    gate.acknowledge(&harness.commit(&batch)?)?;
    assert!(gate.propose(original).is_err());
    let mut unrelated = harness.head.clone();
    unrelated.payload_sha256 = Some(sha256(b"unrelated"));
    assert!(gate.can_dispatch());
    assert!(gate.reconnect(&unrelated).is_err());
    assert!(!gate.can_dispatch());
    let available = BTreeMap::new();
    assert!(
        batch
            .validate_append(
                &grant(),
                &DurableHead {
                    event_seq: 1,
                    ..DurableHead::default()
                },
                &Limits::default(),
                &available,
                None,
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn old_committed_retry_returns_old_ack_without_rewinding_head() -> TestResult {
    let mut harness = DurableHarness::new(grant());
    let first = CheckpointBatch::encode(&proposal(&harness.head, "batch_1"), &harness.limits)?;
    let first_ack = harness.commit(&first)?;
    let second = CheckpointBatch::encode(&proposal(&harness.head, "batch_2"), &harness.limits)?;
    let second_ack = harness.commit(&second)?;
    assert_eq!(harness.commit(&first)?, first_ack);
    assert_eq!(harness.head, second_ack.head());
    assert_eq!(harness.batches.len(), 2);
    let mut altered = first.decode(&harness.limits)?;
    altered.checkpoint.state = json!({"different":true});
    assert!(
        harness
            .commit(&CheckpointBatch::encode(&altered, &harness.limits)?)
            .is_err()
    );
    harness.grant.execution_epoch = 2;
    assert_eq!(
        harness.commit(&first).err().map(|error| error.code),
        Some(ErrorCode::StaleEpoch)
    );
    Ok(())
}

#[test]
fn base64_and_envelope_count_toward_unacknowledged_bound() -> TestResult {
    let limits = Limits {
        checkpoint_bytes: 8192,
        unacknowledged_bytes: 8192,
        input_bytes: 1024,
        ..Limits::default()
    };
    limits.validate()?;
    let mut payload = proposal(&DurableHead::default(), "batch_1");
    payload.checkpoint.state = json!({"content":"a".repeat(7000)});
    let batch = CheckpointBatch::encode(&payload, &Limits::default())?;
    assert!(serde_json::to_vec(&payload)?.len() < 8192);
    assert!(batch.wire_bytes()? > 8192);
    assert_eq!(
        batch.wire_bytes()?,
        serde_json::to_vec(
            &bitrouter_orchestrator::core::protocol::ServerMessage::Checkpoint(batch.clone())
        )?
        .len() as u64
    );
    let mut gate = CommitGate::new(grant(), DurableHead::default(), limits.clone())?;
    assert_eq!(
        gate.propose(payload).err().map(|error| error.code),
        Some(ErrorCode::LimitExceeded)
    );
    assert!(gate.pending().is_none());
    assert!(batch.decode(&limits).is_err());
    Ok(())
}

#[test]
fn inline_material_carries_its_own_media_type() -> TestResult {
    let wire = json!({"version":1,"session_id":"session_1","execution_epoch":1,"operation_id":"material_1","type":"material.result","payload":{"request_id":"request_1","material":{"material_id":"instructions","version":"v1","sha256":sha256(b"instructions"),"media_type":"text/plain","provenance":"harness_observed","required":true,"artifact":null,"content":"instructions"},"unavailable_reason":null}});
    let message: ClientMessage = serde_json::from_value(wire.clone())?;
    message.validate(&grant(), &Limits::default())?;
    assert_eq!(serde_json::to_value(&message)?, wire);
    Ok(())
}

#[test]
fn unknown_wire_fields_are_not_silently_dropped() -> TestResult {
    let mut wire = json!({"version":1,"session_id":"session_1","execution_epoch":1,"operation_id":"cancel_1","expected_state_revision":0,"type":"run.cancel","payload":{"run_id":"run_1"}});
    wire["required_future_feature"] = json!(true);
    assert!(serde_json::from_value::<ClientMessage>(wire.clone()).is_err());
    let object = wire.as_object_mut().ok_or("expected wire object")?;
    object.remove("required_future_feature");
    wire["payload"]["all_sessions"] = json!(true);
    assert!(serde_json::from_value::<ClientMessage>(wire).is_err());
    Ok(())
}
