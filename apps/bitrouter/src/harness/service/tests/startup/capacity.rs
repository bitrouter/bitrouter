use super::*;
use crate::config::startup::StartupConfig;
use bitrouter_ai::harness::runtime::OwnerClaim;
use bitrouter_ai::harness::thread::{PermissionProfile, ThreadEvent, ThreadSnapshot};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;
use tokio::sync::Notify;

#[derive(Default)]
pub(super) struct ReadPause {
    used: AtomicBool,
    entered: Notify,
    release: Notify,
}

impl ReadPause {
    pub(super) async fn wait_once(&self) {
        if !self.used.swap(true, Ordering::AcqRel) {
            self.entered.notify_one();
            self.release.notified().await;
        }
    }
}

// Small canonical roots avoid a history-sized live service in capacity fixtures.
// Missing stopped-owner proof makes selected roots unresolved without effects.
async fn history(
    store: &dyn ExecutionStore,
    workspace: &Path,
    clean_before: usize,
    unresolved: usize,
    clean_after: usize,
    records_per_root: usize,
) -> Result<(), Box<dyn std::error::Error>> {
    let OwnerClaim::Acquired { owner } = store.claim_owner("history-owner").await? else {
        return Err("fixture owner unavailable".into());
    };
    for index in 0..clean_before + unresolved + clean_after {
        let id = format!("root-{index:04}");
        let epoch = if (clean_before..clean_before + unresolved).contains(&index) {
            "unproven-owner"
        } else {
            &owner.server_instance_id
        };
        let mut records = vec![ExecutionRecord::ThreadCreated {
            caller: CallerContext::local(),
            snapshot: ThreadSnapshot {
                server_instance_id: epoch.into(),
                thread_id: id.clone(),
                status: ThreadStatus::Idle,
                workspace: workspace.into(),
                model: "fixture-model".into(),
                permission_profile: PermissionProfile::Ask,
                context_version: 0,
                cursor: 0,
                active_turn_id: None,
                queued: Vec::new(),
                pause_reason: None,
                waiting_for_capacity: false,
            },
            config: Box::new(AgentConfig::fixed("fixture-model", None)),
            verification_command: None,
        }];
        for sequence in 2..=records_per_root {
            records.push(ExecutionRecord::ThreadEvent {
                event: ThreadEvent {
                    server_instance_id: epoch.into(),
                    thread_id: id.clone(),
                    seq: sequence as u64,
                    timestamp_ms: 0,
                    changes: Vec::new(),
                },
            });
        }
        store.commit_owned(&owner, &id, 0, &records).await?;
    }
    store.stop_owner(&owner).await?;
    Ok(())
}

fn fault(store: Arc<MemoryExecutionStore>, batch: usize) -> DiscoveryFault {
    DiscoveryFault {
        memory: store,
        fail_index: AtomicBool::new(false),
        corrupt_result: false,
        max_record_limit: Some(batch),
        pause: None,
    }
}

#[tokio::test]
async fn clean_history_exceeds_root_metadata_and_record_budgets()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let store = Arc::new(MemoryExecutionStore::default());
    history(store.as_ref(), workspace.path(), 130, 0, 0, 19).await?;
    let config = crate::config::parse_with(
        "inherit_defaults: false\nagent_api:\n  enabled: false\n  startup: {unresolved_roots: 1, unresolved_metadata_bytes: 1, records_per_batch: 3}\n",
        |_| None,
    )?;
    let reader = ThreadService::with_startup_and_store(
        app(vec![final_turn()])?,
        &[workspace.path().into()],
        &config.agent_api.startup,
        Arc::new(fault(store, 3)),
    )?;
    reader.initialize_execution().await?;
    let capabilities = reader.capabilities();
    assert_eq!(capabilities.limits.startup_roots, 1);
    assert_eq!(capabilities.limits.startup_metadata_bytes, 1);
    assert_eq!(capabilities.limits.startup_records, 3);
    let report = capabilities.startup_discovery.ok_or("discovery missing")?;
    assert!(report.complete && report.writer_fenced);
    assert_eq!(report.inspected_roots, 130);
    assert_eq!(report.scanned_records, 130 * 19);
    assert_eq!(report.blocked_workspaces, 0);
    assert!(reader.lock_state().cold_executions.is_empty());
    assert!(reader.lock_state().threads.is_empty());
    let fresh = reader
        .create_thread(
            &reader.inner.instance_id,
            thread_request(&workspace, "fresh"),
        )
        .await?;
    let turn = reader
        .start_turn(
            &target(&fresh),
            &CallerContext::local(),
            input("fresh", "fresh-turn"),
        )
        .await?;
    wait_for(&reader, &turn.turn_id, TurnStatus::Completed).await?;
    reader.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn full_unresolved_root_budget_still_audits_later_clean_roots()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    for unresolved in [0, 1, 2] {
        let store = Arc::new(MemoryExecutionStore::default());
        history(store.as_ref(), workspace.path(), 65, unresolved, 65, 5).await?;
        let reader = ThreadService::with_startup_and_store(
            app(vec![])?,
            &[workspace.path().into()],
            &StartupConfig {
                unresolved_roots: 1,
                unresolved_metadata_bytes: 4096,
                records_per_batch: 1,
            },
            Arc::new(fault(store, 1)),
        )?;
        let result = reader.initialize_execution().await;
        let report = reader
            .capabilities()
            .startup_discovery
            .ok_or("discovery missing")?;
        if unresolved <= 1 {
            result?;
            assert!(report.complete);
            assert_eq!(report.inspected_roots, 130 + unresolved);
            assert_eq!(report.scanned_records, ((130 + unresolved) * 5) as u64);
            assert_eq!(report.blocked_workspaces, unresolved);
            assert_eq!(reader.lock_state().cold_executions.len(), unresolved);
        } else {
            assert_eq!(
                result.err().ok_or("unresolved budget bypassed")?.code,
                ErrorCode::Overloaded
            );
            assert!(!report.complete);
            assert!(reader.lock_state().cold_executions.is_empty());
        }
        reader.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn unresolved_metadata_accounts_for_key_path_and_map_overhead()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = TempDir::new()?;
    let bytes = crate::config::startup::UNRESOLVED_ENTRY_OVERHEAD
        + "root-0000".len()
        + workspace.path().as_os_str().len();
    for budget in [bytes - 1, bytes, bytes + 1] {
        let store = Arc::new(MemoryExecutionStore::default());
        history(store.as_ref(), workspace.path(), 0, 1, 65, 1).await?;
        let reader = ThreadService::with_startup_and_store(
            app(vec![])?,
            &[workspace.path().into()],
            &StartupConfig {
                unresolved_roots: 1,
                unresolved_metadata_bytes: budget,
                records_per_batch: 1,
            },
            Arc::new(fault(store, 1)),
        )?;
        let result = reader.initialize_execution().await;
        if budget < bytes {
            assert_eq!(
                result.err().ok_or("metadata budget bypassed")?.code,
                ErrorCode::Overloaded
            );
            assert!(reader.lock_state().cold_executions.is_empty());
        } else {
            result?;
            let report = reader
                .capabilities()
                .startup_discovery
                .ok_or("discovery missing")?;
            assert!(report.complete);
            assert_eq!(report.inspected_roots, 66);
            assert_eq!(report.blocked_workspaces, 1);
        }
        reader.shutdown().await;
    }
    Ok(())
}

#[tokio::test]
async fn interrupted_discovery_never_publishes_ready_and_can_retry()
-> Result<(), Box<dyn std::error::Error>> {
    for shutdown in [false, true] {
        let workspace = TempDir::new()?;
        let store = Arc::new(MemoryExecutionStore::default());
        history(store.as_ref(), workspace.path(), 2, 0, 0, 9).await?;
        let pause = Arc::new(ReadPause::default());
        let mut store = fault(store, 2);
        store.pause = Some(pause.clone());
        let reader = ThreadService::with_startup_and_store(
            app(vec![])?,
            &[workspace.path().into()],
            &StartupConfig {
                records_per_batch: 2,
                ..StartupConfig::default()
            },
            Arc::new(store),
        )?;
        let worker = reader.clone();
        let scan = tokio::spawn(async move { worker.initialize_execution().await });
        tokio::time::timeout(Duration::from_secs(5), pause.entered.notified()).await?;
        if shutdown {
            let worker = reader.clone();
            let stop = tokio::spawn(async move { worker.shutdown().await });
            tokio::time::timeout(Duration::from_secs(5), async {
                while !reader.lock_state().closing {
                    tokio::task::yield_now().await;
                }
            })
            .await?;
            pause.release.notify_one();
            let result = tokio::time::timeout(Duration::from_secs(5), scan).await??;
            assert_eq!(
                result.err().ok_or("shutdown scan succeeded")?.code,
                ErrorCode::ShuttingDown
            );
            tokio::time::timeout(Duration::from_secs(5), stop).await??;
        } else {
            scan.abort();
            assert!(scan.await.err().is_some_and(|error| error.is_cancelled()));
        }
        assert!(
            !reader
                .capabilities()
                .startup_discovery
                .ok_or("discovery missing")?
                .complete
        );
        assert!(reader.lock_state().cold_executions.is_empty());
        if !shutdown {
            reader.initialize_execution().await?;
            assert!(
                reader
                    .capabilities()
                    .startup_discovery
                    .ok_or("discovery missing")?
                    .complete
            );
            reader.shutdown().await;
        }
    }
    Ok(())
}
