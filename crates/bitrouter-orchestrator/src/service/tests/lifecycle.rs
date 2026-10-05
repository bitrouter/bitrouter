use std::sync::Arc;
use std::time::Duration;

use tempfile::TempDir;

use super::support::*;
use crate::item::CallOrigin;
use crate::service::{ErrorCode, RuntimeLimits, ThreadService};
use crate::store::{EffectStatus, ExecutionRecord, ExecutionStore, MemoryExecutionStore};
use crate::turn::{TurnStatus, VerificationStatus};

#[tokio::test]
async fn approval_is_task_bound_and_one_use() -> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let other = TempDir::new()?;
    let app = app(vec![
        turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"created.txt", "content":"created"}),
        )]),
        final_turn(),
    ])?;
    let service = ThreadService::new(app, &[workspace.path().to_path_buf()])
        .map_err(std::io::Error::other)?;
    assert!(service.submit_fixture(request(&other)).await.is_err());
    let accepted = service
        .submit_fixture(request(&workspace))
        .await
        .map_err(std::io::Error::other)?;
    assert!(service.submit_fixture(request(&workspace)).await.is_err());
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput)
        .await
        .map_err(std::io::Error::other)?;
    assert_eq!(waiting.status, TurnStatus::WaitingForInput);
    let input_id = waiting
        .pending_input_id
        .ok_or_else(|| std::io::Error::other("missing pending input"))?;
    assert!(
        service
            .answer_input(&accepted.turn_id, "wrong", true)
            .await
            .is_err()
    );
    assert!(!workspace.path().join("created.txt").exists());
    service
        .answer_input(&accepted.turn_id, &input_id, true)
        .await
        .map_err(std::io::Error::other)?;
    assert!(
        service
            .answer_input(&accepted.turn_id, &input_id, true)
            .await
            .is_err()
    );
    let completed = wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
        .await
        .map_err(std::io::Error::other)?;
    assert_eq!(completed.status, TurnStatus::Completed);
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("created.txt"))?,
        "created"
    );
    Ok(())
}

#[tokio::test]
async fn cancellation_while_waiting_never_authorizes_a_write()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"created.txt", "content":"created"}),
        )])])?,
        &[workspace.path().to_path_buf()],
    )
    .map_err(std::io::Error::other)?;
    let accepted = service
        .submit_fixture(request(&workspace))
        .await
        .map_err(std::io::Error::other)?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput)
        .await
        .map_err(std::io::Error::other)?;
    let input_id = waiting
        .pending_input_id
        .ok_or_else(|| std::io::Error::other("missing pending input"))?;
    service
        .cancel(&accepted.turn_id)
        .await
        .map_err(std::io::Error::other)?;
    assert!(
        service
            .answer_input(&accepted.turn_id, &input_id, true)
            .await
            .is_err()
    );
    let cancelled = wait_for(&service, &accepted.turn_id, TurnStatus::Cancelled)
        .await
        .map_err(std::io::Error::other)?;
    assert_eq!(cancelled.status, TurnStatus::Cancelled);
    assert!(!workspace.path().join("created.txt").exists());
    Ok(())
}

#[tokio::test]
async fn shutdown_seals_admission_and_joins_waiting_execution()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let other = TempDir::new()?;
    let limits = RuntimeLimits {
        active_turns: 1,
        ..RuntimeLimits::default()
    };
    let service = ThreadService::with_limits(
        app(vec![turn(vec![tool_call(
            "write",
            "write",
            serde_json::json!({"path":"created.txt", "content":"created"}),
        )])])?,
        &[workspace.path().to_path_buf(), other.path().to_path_buf()],
        limits,
    )?;
    let accepted = service.submit_fixture(request(&workspace)).await?;
    wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    assert_eq!(
        service
            .submit_fixture(request(&other))
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::Overloaded)
    );
    tokio::time::timeout(Duration::from_secs(2), service.shutdown()).await?;
    assert_eq!(
        service.read(&accepted.turn_id)?.status,
        TurnStatus::Cancelled
    );
    assert!(service.read(&accepted.turn_id)?.pending_input.is_none());
    assert!(service.inner.workers.is_empty());
    assert!(service.lock_state().active_workspaces.is_empty());
    assert_eq!(
        service
            .submit_fixture(request(&workspace))
            .await
            .err()
            .map(|error| error.code),
        Some(ErrorCode::ShuttingDown)
    );
    assert!(!workspace.path().join("created.txt").exists());
    Ok(())
}

#[tokio::test]
async fn terminal_cache_eviction_preserves_durable_acceptance_keys()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::with_limits(
        app(vec![final_turn(), final_turn(), final_turn()])?,
        &[workspace.path().to_path_buf()],
        RuntimeLimits {
            retained_turns: 1,
            ..RuntimeLimits::default()
        },
    )?;
    let mut first = request(&workspace);
    first.idempotency_key = Some("first".into());
    let first = service.submit_fixture(first).await?;
    wait_for(&service, &first.turn_id, TurnStatus::Completed).await?;
    let second = service.submit_fixture(request(&workspace)).await?;
    wait_for(&service, &second.turn_id, TurnStatus::Completed).await?;
    assert_eq!(
        service.read(&first.turn_id).err().map(|error| error.code),
        Some(ErrorCode::UnknownTurn)
    );
    let mut reused = request(&workspace);
    reused.idempotency_key = Some("first".into());
    assert_eq!(service.submit_fixture(reused).await?.turn_id, first.turn_id);
    service.shutdown().await;
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn shutdown_waits_for_verification_process_cleanup() -> Result<(), Box<dyn std::error::Error>>
{
    let workspace = TempDir::new()?;
    let service = ThreadService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
    let mut submitted = request(&workspace);
    submitted.verification_command = Some("touch started; sleep 30; touch leaked".into());
    let accepted = service.submit_fixture(submitted).await?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    service
        .answer_input(
            &accepted.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("verification approval missing")?,
            true,
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(3), async {
        while !workspace.path().join("started").exists() {
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    tokio::time::timeout(Duration::from_secs(3), service.shutdown()).await?;
    assert!(service.inner.workers.is_empty());
    assert_eq!(
        service.read(&accepted.turn_id)?.status,
        TurnStatus::RecoveryRequired
    );
    assert!(service.read(&accepted.turn_id)?.unknown_effect);
    assert!(!workspace.path().join("leaked").exists());
    Ok(())
}

#[tokio::test]
async fn configured_verification_records_exit_status_and_controls_outcome()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(
        app(vec![final_turn(), final_turn()])?,
        &[workspace.path().to_path_buf()],
    )
    .map_err(std::io::Error::other)?;
    let mut read_only = request(&workspace);
    read_only.config = read_only.config.read_only();
    read_only.verification_command = Some("echo forbidden".into());
    assert!(service.submit_fixture(read_only).await.is_err());
    let mut passing = request(&workspace);
    passing.verification_command = Some("echo verified".into());
    let accepted = service
        .submit_fixture(passing)
        .await
        .map_err(std::io::Error::other)?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    assert_eq!(
        service.inner.tool_workers.available_permits(),
        service.inner.limits.global_tools
    );
    service
        .answer_input(
            &accepted.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("verification approval missing")?,
            true,
        )
        .await?;
    let passed = wait_for(&service, &accepted.turn_id, TurnStatus::Completed)
        .await
        .map_err(std::io::Error::other)?;
    assert_eq!(passed.status, TurnStatus::Completed);
    assert_eq!(passed.verification, VerificationStatus::Passed);
    let stored = service
        .inner
        .store
        .load(&accepted.thread_id)
        .await?
        .ok_or("execution records missing")?;
    assert!(stored.records.iter().any(|record| matches!(turn_fact(record), ExecutionRecord::ToolIntent { call, .. } if call.origin == CallOrigin::Verification)));
    assert!(stored.records.iter().any(|record| matches!(turn_fact(record), ExecutionRecord::VerificationResult { call, effect: EffectStatus::Completed, evidence, .. } if call.origin == CallOrigin::Verification && call.name == "shell" && evidence.exit_status == Some(0))));
    let interpreter = passed
        .verification_evidence
        .as_ref()
        .and_then(|evidence| evidence.interpreter.as_ref())
        .ok_or("verification interpreter missing")?;
    let executable = interpreter["executable"]
        .as_str()
        .ok_or("executable missing")?;
    assert!(stored.records.iter().any(|record| matches!(turn_fact(record),
        ExecutionRecord::ModelRequest { prompt, .. } if prompt.tools.iter().any(|tool|
            matches!(tool, bitrouter_sdk::language_model::Tool::Function { name, description: Some(description), .. }
                if name == "shell" && description.contains(executable))))));
    assert_eq!(
        passed
            .verification_evidence
            .as_ref()
            .and_then(|e| e.exit_status),
        Some(0)
    );
    let mut failing = request(&workspace);
    failing.verification_command = Some("exit 7".into());
    let accepted = service
        .submit_fixture(failing)
        .await
        .map_err(std::io::Error::other)?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    service
        .answer_input(
            &accepted.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("verification approval missing")?,
            true,
        )
        .await?;
    let failed = wait_for(&service, &accepted.turn_id, TurnStatus::Failed)
        .await
        .map_err(std::io::Error::other)?;
    assert_eq!(failed.status, TurnStatus::Failed);
    assert_eq!(failed.verification, VerificationStatus::Failed);
    assert_eq!(
        failed
            .verification_evidence
            .as_ref()
            .and_then(|e| e.exit_status),
        Some(7)
    );
    Ok(())
}

#[tokio::test]
async fn verification_denial_and_permit_cancellation_never_launch_a_command()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let service = ThreadService::new(
        app(vec![final_turn(), final_turn()])?,
        &[workspace.path().to_path_buf()],
    )?;
    let permits = Arc::clone(&service.inner.tool_workers)
        .acquire_many_owned(16)
        .await?;
    for approved in [false, true] {
        let mut submitted = request(&workspace);
        submitted.verification_command = Some("echo blocked > created".into());
        let accepted = service.submit_fixture(submitted).await?;
        let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
        let input = waiting
            .pending_input
            .as_ref()
            .ok_or("verification approval missing")?;
        assert!(input.arguments.contains("echo blocked > created"));
        service
            .answer_input(&accepted.turn_id, &input.request_id, approved)
            .await?;
        if approved {
            service.cancel(&accepted.turn_id).await?;
        }
        let finished = wait_for(
            &service,
            &accepted.turn_id,
            if approved {
                TurnStatus::Cancelled
            } else {
                TurnStatus::Failed
            },
        )
        .await?;
        assert_eq!(
            finished.verification,
            if approved {
                VerificationStatus::Unavailable
            } else {
                VerificationStatus::Denied
            }
        );
        assert!(!finished.unknown_effect);
        assert!(!workspace.path().join("created").exists());
        let stored = service
            .inner
            .store
            .load(&accepted.thread_id)
            .await?
            .ok_or("execution missing")?;
        assert!(!stored.records.iter().any(|record| matches!(turn_fact(record), ExecutionRecord::ToolIntent { call, .. } if call.origin == CallOrigin::Verification)));
        assert!(stored.records.iter().any(|record| matches!(
            turn_fact(record),
            ExecutionRecord::VerificationResult {
                effect: EffectStatus::NotExecuted,
                ..
            }
        )));
    }
    drop(permits);
    service.shutdown().await;
    Ok(())
}

struct FailingStore {
    memory: MemoryExecutionStore,
    failure: &'static str,
}

#[async_trait::async_trait]
impl ExecutionStore for FailingStore {
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<crate::store::ExecutionIndexPage, String> {
        self.memory
            .read_index(after, cutoff, limit, max_bytes)
            .await
    }

    async fn claim_owner(&self, id: &str) -> Result<crate::store::OwnerClaim, String> {
        self.memory.claim_owner(id).await
    }
    async fn read_owner(&self, id: &str) -> Result<Option<crate::store::ExecutionOwner>, String> {
        self.memory.read_owner(id).await
    }
    async fn stop_owner(
        &self,
        owner: &crate::store::ExecutionOwner,
    ) -> Result<crate::store::ExecutionOwner, String> {
        self.memory.stop_owner(owner).await
    }

    async fn commit(
        &self,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        self.memory.commit(id, version, records).await
    }

    async fn read_records(
        &self,
        id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Option<crate::store::ExecutionPage>, String> {
        self.memory
            .read_records(id, after, cutoff, limit, max_bytes)
            .await
    }

    async fn thread_history(
        &self,
        id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<crate::store::ThreadHistoryChunk, String> {
        self.memory
            .thread_history(id, after, cutoff, limit, max_bytes)
            .await
    }

    async fn commit_owned(
        &self,
        owner: &crate::store::ExecutionOwner,
        id: &str,
        version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        let fails = records.iter().any(|record| match turn_fact(record) {
            ExecutionRecord::ThreadCreated { .. } => self.failure == "accepted",
            ExecutionRecord::ModelResponse { .. } => self.failure == "response",
            ExecutionRecord::HarnessInventory { .. } => self.failure == "inventory",
            ExecutionRecord::ToolIntent { .. } => self.failure == "intent",
            ExecutionRecord::ToolResult { .. } => self.failure == "result",
            ExecutionRecord::WorkspaceReleasePrepared { .. } => self.failure == "release",
            _ => false,
        });
        if fails {
            return Err("injected execution commit failure".into());
        }
        self.memory.commit_owned(owner, id, version, records).await
    }
    async fn load(&self, id: &str) -> Result<Option<crate::store::StoredExecution>, String> {
        self.memory.load(id).await
    }
    async fn find_key(
        &self,
        scope: &str,
        key: &str,
    ) -> Result<Option<crate::store::AcceptedKey>, String> {
        self.memory.find_key(scope, key).await
    }
}

#[tokio::test]
async fn commit_failures_block_effects_and_preserve_uncertain_execution()
-> Result<(), Box<dyn std::error::Error>> {
    for failure in ["accepted", "inventory", "response", "intent", "result"] {
        let workspace = TempDir::new()?;
        let store = Arc::new(FailingStore {
            memory: MemoryExecutionStore::default(),
            failure,
        });
        let service = ThreadService::with_store(
            app(vec![
                turn(vec![
                    tool_call(
                        "first",
                        "write",
                        serde_json::json!({"path":"first.txt","content":"one"}),
                    ),
                    tool_call(
                        "second",
                        "write",
                        serde_json::json!({"path":"second.txt","content":"two"}),
                    ),
                ]),
                final_turn(),
            ])?,
            &[workspace.path().to_path_buf()],
            store.clone(),
        )?;
        let accepted = service.submit_fixture(request(&workspace)).await;
        if failure == "accepted" {
            assert_eq!(
                accepted.err().map(|error| error.code),
                Some(ErrorCode::StorageUnavailable)
            );
            assert!(!workspace.path().join("first.txt").exists());
            service.shutdown().await;
            assert!(
                store
                    .memory
                    .read_owner(&service.inner.instance_id)
                    .await?
                    .ok_or("owner missing")?
                    .stopped_at_ms
                    .is_none()
            );
            continue;
        }
        let accepted = accepted?;
        if failure != "response" && failure != "inventory" {
            let approval =
                wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
            let request_id = approval
                .pending_input_id
                .ok_or("approval identity missing")?;
            service
                .answer_input(&accepted.turn_id, &request_id, true)
                .await?;
        }
        let blocked = wait_for(&service, &accepted.turn_id, TurnStatus::RecoveryRequired).await?;
        assert!(blocked.unknown_effect);
        service.shutdown().await;
        assert!(
            store
                .memory
                .read_owner(&service.inner.instance_id)
                .await?
                .ok_or("owner missing")?
                .stopped_at_ms
                .is_none()
        );
        assert_eq!(
            workspace.path().join("first.txt").exists(),
            failure == "result"
        );
        assert!(!workspace.path().join("second.txt").exists());
        let saved = store
            .load(&accepted.thread_id)
            .await?
            .ok_or("execution missing")?;
        assert!(
            !saved
                .records
                .iter()
                .any(|record| matches!(turn_fact(record), ExecutionRecord::ToolResult { .. }))
        );
        if failure == "inventory" {
            assert!(
                super::support::prompts(store.as_ref(), &accepted.thread_id, &accepted.turn_id)
                    .await?
                    .is_empty()
            );
        }
        if failure == "result" {
            assert!(
                saved
                    .records
                    .iter()
                    .any(|record| matches!(turn_fact(record), ExecutionRecord::ToolIntent { .. }))
            );
        }
        assert!(
            service
                .lock_state()
                .active_workspaces
                .contains_key(&blocked.workspace)
        );
    }
    Ok(())
}

#[tokio::test]
async fn release_preparation_failure_retains_exclusion_without_publishing_completion()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(FailingStore {
        memory: MemoryExecutionStore::default(),
        failure: "release",
    });
    let service = ThreadService::with_store(
        app(vec![
            turn(vec![tool_call(
                "write",
                "write",
                serde_json::json!({"path":"known.txt", "content":"one"}),
            )]),
            final_turn(),
        ])?,
        &[workspace.path().to_path_buf()],
        store.clone(),
    )?;
    let accepted = service.submit_fixture(request(&workspace)).await?;
    let waiting = wait_for(&service, &accepted.turn_id, TurnStatus::WaitingForInput).await?;
    service
        .answer_input(
            &accepted.turn_id,
            waiting
                .pending_input_id
                .as_deref()
                .ok_or("approval missing")?,
            true,
        )
        .await?;
    assert_eq!(
        wait_for(&service, &accepted.turn_id, TurnStatus::RecoveryRequired)
            .await?
            .status,
        TurnStatus::RecoveryRequired
    );
    assert_eq!(
        std::fs::read_to_string(workspace.path().join("known.txt"))?,
        "one"
    );
    let saved = store
        .load(&accepted.thread_id)
        .await?
        .ok_or("execution missing")?;
    assert!(saved.records.iter().any(|r| matches!(
        turn_fact(r),
        ExecutionRecord::ToolResult {
            effect: EffectStatus::Completed,
            ..
        }
    )));
    assert!(!saved.records.iter().any(|r| matches!(
        turn_fact(r),
        ExecutionRecord::TurnLifecycle {
            lifecycle: crate::turn::TurnLifecycle::Finished { .. },
            ..
        }
    )));
    service.shutdown().await;
    assert!(
        store
            .read_owner(&service.inner.instance_id)
            .await?
            .ok_or("owner missing")?
            .stopped_at_ms
            .is_none()
    );
    drop(service);
    let peer = ThreadService::new(app(vec![final_turn()])?, &[workspace.path().to_path_buf()])?;
    assert_eq!(
        peer.submit_fixture(request(&workspace))
            .await
            .err()
            .ok_or("unknown release was bypassed")?
            .code,
        ErrorCode::RecoveryRequired
    );
    peer.shutdown().await;
    Ok(())
}
