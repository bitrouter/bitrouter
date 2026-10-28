use super::*;
use crate::harness::agent::AgentConfigExt;
use crate::harness::storage::MemoryExecutionStore;
use bitrouter_ai::harness::agent::AgentConfig;
use bitrouter_ai::harness::thread::{PermissionProfile, ThreadSnapshot, ThreadStatus};

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn header() -> ExecutionRecord {
    ExecutionRecord::ThreadCreated {
        caller: CallerContext::local(),
        snapshot: ThreadSnapshot {
            server_instance_id: "original-owner".into(),
            thread_id: "original-thread".into(),
            status: ThreadStatus::Idle,
            workspace: "/missing/historical-workspace".into(),
            model: "fixture-model".into(),
            permission_profile: PermissionProfile::ReadOnly,
            context_version: 0,
            cursor: 1,
            active_turn_id: None,
            queued: Vec::new(),
            pause_reason: None,
            waiting_for_capacity: false,
        },
        config: Box::new(AgentConfig::fixed("fixture-model", None).read_only()),
        verification_command: None,
    }
}

fn queued() -> ExecutionRecord {
    ExecutionRecord::TurnQueued {
        turn_id: "original-turn".into(),
        user_item_id: "user-input".into(),
        prompt: "original prompt".into(),
        queue_order: 1,
    }
}

#[tokio::test]
async fn cold_inspection_preserves_ownership_and_queued_identity_without_a_workspace_or_app()
-> TestResult {
    let store = MemoryExecutionStore::default();
    store
        .commit("original-thread", 0, &[header(), queued()])
        .await?;
    let before = store
        .load("original-thread")
        .await?
        .ok_or("original store")?;
    let report = inspect(
        &store,
        &CallerContext::local(),
        "original-thread",
        "original-turn",
    )
    .await?;
    assert_eq!(report.cutoff, before.version);
    assert_eq!(report.recorded_server_instance_id, "original-owner");
    assert!(report.explanation.is_none());
    assert!(
        inspect(
            &store,
            &CallerContext::anonymous(),
            "original-thread",
            "original-turn"
        )
        .await
        .is_err()
    );
    assert!(
        inspect(
            &store,
            &CallerContext::new("other-key", "other-user"),
            "original-thread",
            "original-turn"
        )
        .await
        .is_err()
    );
    assert!(
        inspect(
            &store,
            &CallerContext::local(),
            "original-thread",
            "another-turn"
        )
        .await
        .is_err()
    );
    let after = store
        .load("original-thread")
        .await?
        .ok_or("retained store")?;
    assert_eq!(before.version, after.version);
    assert_eq!(
        serde_json::to_value(before.records)?,
        serde_json::to_value(after.records)?
    );
    assert!(store.read_owner("original-owner").await?.is_none());
    Ok(())
}

#[tokio::test]
async fn cold_inspection_rejects_a_truncated_checkpoint_instead_of_reporting_empty_selection()
-> TestResult {
    use base64::Engine;
    let store = MemoryExecutionStore::default();
    let bytes = b"incomplete checkpoint";
    let part = ExecutionRecord::CoreCheckpointPart {
        sha256: crate::storage::checkpoint::sha256(bytes),
        bytes: bytes.len() as u64,
        offset: 0,
        content_base64: base64::engine::general_purpose::STANDARD.encode(&bytes[..4]),
    };
    store
        .commit("original-thread", 0, &[header(), queued(), part])
        .await?;
    let error = inspect(
        &store,
        &CallerContext::local(),
        "original-thread",
        "original-turn",
    )
    .await
    .err()
    .ok_or("incomplete evidence was accepted")?;
    assert!(error.contains("incomplete checkpoint/projection"));
    Ok(())
}
