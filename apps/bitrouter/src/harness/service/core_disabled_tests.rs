//! Core-free profiles may inspect durable work, but cannot acquire its authority.

use super::*;
use crate::gateway::caller::CallerContext;
use crate::harness::agent::Agent;
use crate::harness::agent::AgentConfigExt;
use crate::harness::storage::ExecutionRecord;
use crate::harness::thread::ThreadRequest;
use crate::harness::thread::ThreadTarget;
use crate::harness::turn::TurnRequest;
use bitrouter_ai::harness::agent::AgentConfig;
use bitrouter_ai::harness::thread::ThreadSnapshot;
use bitrouter_ai::harness::thread::ThreadStatus;

type TestResult = Result<(), Box<dyn std::error::Error>>;

fn unavailable<T>(result: Result<T, ServiceError>) -> TestResult {
    let error = result
        .err()
        .ok_or("native operation was admitted without core")?;
    assert_eq!(error.code, ErrorCode::UnsupportedCapability);
    assert_eq!(error.message, crate::harness::agent::NATIVE_UNAVAILABLE);
    Ok(())
}

#[tokio::test]
async fn native_admission_rejects_before_workspace_lookup_or_store_ownership() -> TestResult {
    let workspace = tempfile::tempdir()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let app = Arc::new(App::builder().build()?);
    let service =
        ThreadService::with_store(app.clone(), &[workspace.path().into()], store.clone())?;
    let capabilities = service.capabilities();
    assert!(!capabilities.native_execution);
    assert!(capabilities.execution_ownership.is_none());
    let epoch = capabilities.server_instance_id;
    let missing_workspace = workspace.path().join("must-not-be-discovered");
    let rejected = Agent::new(
        app,
        CallerContext::local(),
        &missing_workspace,
        AgentConfig::fixed("fixture", None),
    );
    assert_eq!(
        rejected.err().as_deref(),
        Some(crate::harness::agent::NATIVE_UNAVAILABLE)
    );
    unavailable(service.initialize_execution().await)?;
    unavailable(
        service
            .create_thread(
                &epoch,
                ThreadRequest {
                    caller: CallerContext::local(),
                    workspace: missing_workspace,
                    config: AgentConfig::fixed("fixture", None),
                    permission_profile: PermissionProfile::Ask,
                    verification_command: None,
                    idempotency_key: "new-native-thread".into(),
                },
            )
            .await,
    )?;
    let target = ThreadTarget {
        thread_id: "not-created".into(),
        server_instance_id: epoch.clone(),
    };
    unavailable(
        service
            .start_turn(
                &target,
                &CallerContext::local(),
                TurnRequest {
                    prompt: "must not execute".into(),
                    idempotency_key: "new-native-turn".into(),
                },
            )
            .await,
    )?;
    unavailable(
        service
            .resume_queue(&target, &CallerContext::local(), "resume".into())
            .await,
    )?;
    unavailable(
        service
            .recover_thread(
                &target,
                &CallerContext::local(),
                bitrouter_ai::harness::thread::ThreadRecoveryRequest {
                    source_server_instance_id: "prior".into(),
                    source_cursor: 1,
                    idempotency_key: "recover".into(),
                },
            )
            .await,
    )?;
    unavailable(
        service
            .close_thread(&target, &CallerContext::local(), "close".into())
            .await,
    )?;
    assert!(store.read_owner(&epoch).await?.is_none());
    assert!(
        store
            .read_index(0, None, 16, 1024 * 1024)
            .await?
            .entries
            .is_empty()
    );
    assert_eq!(std::fs::read_dir(workspace.path())?.count(), 0);
    service.shutdown().await;
    Ok(())
}

pub(crate) struct OfflineFixture {
    pub(crate) workspace: tempfile::TempDir,
    pub(crate) store: Arc<MemoryExecutionStore>,
    pub(crate) service: ThreadService,
    pub(crate) target: ThreadTarget,
}

pub(crate) async fn offline_fixture(
    caller: CallerContext,
) -> Result<OfflineFixture, Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let root = workspace.path().canonicalize()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let app = Arc::new(App::builder().build()?);
    let writer =
        ThreadService::with_store(app.clone(), std::slice::from_ref(&root), store.clone())?;
    let snapshot = ThreadSnapshot {
        server_instance_id: writer.capabilities().server_instance_id,
        thread_id: "stored-thread".into(),
        status: ThreadStatus::Idle,
        workspace: root.clone(),
        model: "fixture".into(),
        permission_profile: PermissionProfile::ReadOnly,
        context_version: 0,
        cursor: 0,
        active_turn_id: None,
        queued: Vec::new(),
        pause_reason: None,
        waiting_for_capacity: false,
    };
    let records = vec![
        ExecutionRecord::ThreadCreated {
            caller,
            snapshot: snapshot.clone(),
            config: Box::new(AgentConfig::fixed("fixture", None).read_only()),
            verification_command: None,
        },
        ExecutionRecord::TurnQueued {
            turn_id: "queued-turn".into(),
            user_item_id: "queued-input".into(),
            prompt: "preserve queued work".into(),
            queue_order: 1,
        },
    ];
    // Cold HTTP reads require the committed public cutoff as well as private
    // facts. Use the writer's production projection, then reopen in a new epoch.
    let (records, _) = writer.thread_transaction(&snapshot.thread_id, 0, &records, None)?;
    store.commit(&snapshot.thread_id, 0, &records).await?;
    drop(writer);
    let service = ThreadService::with_store(app, &[root], store.clone())?;
    let target = ThreadTarget {
        thread_id: snapshot.thread_id,
        server_instance_id: service.capabilities().server_instance_id,
    };
    Ok(OfflineFixture {
        workspace,
        store,
        service,
        target,
    })
}

#[tokio::test]
async fn offline_load_retains_queued_records_without_claiming_workspace_or_resuming() -> TestResult
{
    let OfflineFixture {
        workspace,
        store,
        service,
        target,
    } = offline_fixture(CallerContext::local()).await?;
    let before = serde_json::to_vec(&store.load(&target.thread_id).await?)?;
    for _ in 0..2 {
        let view = service
            .load_thread(&target, &CallerContext::local())
            .await?;
        let recovery = view
            .recovery
            .as_ref()
            .ok_or("offline audit omitted its recovery report")?;
        assert!(recovery.context_valid, "{:?}", recovery.blockers);
        assert_eq!(view.thread.queued.len(), 1);
        assert_eq!(view.thread.status, ThreadStatus::RecoveryRequired);
    }
    let mut observer = service.observe_thread(&target, &CallerContext::local(), None)?;
    let observation = tokio::time::timeout(Duration::from_secs(2), observer.next()).await??;
    assert!(
        matches!(observation, Some(bitrouter_ai::harness::thread::ThreadObservation::Snapshot { view, .. })
        if view.thread.thread_id == target.thread_id && view.thread.queued.len() == 1)
    );
    assert!(
        service
            .unload_thread(&target, &CallerContext::local())
            .await
            .is_err()
    );
    drop(observer);
    service.drive_queues().await;
    unavailable(
        service
            .resume_queue(&target, &CallerContext::local(), "offline-resume".into())
            .await,
    )?;
    {
        let state = service.lock_state();
        assert_eq!(state.threads.len(), 1);
        assert!(state.workspace_fences.is_empty());
        assert!(state.active_workspaces.is_empty());
        assert!(state.execution_ownership.is_none());
    }
    assert!(
        store
            .read_owner(&target.server_instance_id)
            .await?
            .is_none()
    );
    service
        .unload_thread(&target, &CallerContext::local())
        .await?;
    assert!(service.lock_state().threads.is_empty());
    service
        .load_thread(&target, &CallerContext::local())
        .await?;
    service.shutdown().await;
    assert_eq!(
        before,
        serde_json::to_vec(&store.load(&target.thread_id).await?)?
    );
    assert_eq!(std::fs::read_dir(workspace.path())?.count(), 0);
    Ok(())
}

#[test]
fn capabilities_keep_the_legacy_native_default_when_reading_old_peers() -> TestResult {
    let service = ThreadService::new(Arc::new(App::builder().build()?), &[])?;
    let mut value = serde_json::to_value(service.capabilities())?;
    assert_eq!(value["native_execution"], false);
    value
        .as_object_mut()
        .ok_or("capability object missing")?
        .remove("native_execution");
    let legacy: RuntimeCapabilities = serde_json::from_value(value)?;
    assert!(legacy.native_execution);
    Ok(())
}
