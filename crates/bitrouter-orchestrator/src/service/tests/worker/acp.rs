//! Shared dispatch fences with a deliberately held adapter startup. Actual ACP
//! processes are covered separately by the app's Rust protocol fixtures.
use super::*;
use crate::service::worker::acp::{Binding, Driver, Observation, Owner, Stop};
use crate::thread::executor::{AcpConfig, AcpThreadRequest};
use std::time::Duration;
use tokio::sync::Notify;

async fn worker_thread(
    service: &ThreadService,
    directory: &tempfile::TempDir,
    execution_id: &str,
) -> Result<(GoalWork, ThreadSnapshot), Box<dyn std::error::Error>> {
    let root = root(service, directory).await?;
    let source = GoalWork {
        owner_thread_id: root.thread_id.clone(),
        goal_id: "goal".into(),
        task_id: "work".into(),
        execution_id: execution_id.into(),
        task_revision: 1,
        control_source_revision: 0,
    };
    let thread = service
        .create_goal_acp_thread(
            &target(&root),
            &source,
            AcpThreadRequest {
                caller: CallerContext::local(),
                workspace: directory.path().into(),
                config: AcpConfig {
                    agent_id: "test-peer".into(),
                    requested_model_route: None,
                    max_duration: Duration::from_secs(10),
                },
                permission_profile: PermissionProfile::Ask,
                idempotency_key: crate::service::worker::thread_key(&source)?,
            },
        )
        .await?;
    Ok((source, thread))
}

#[derive(Default)]
struct Held {
    entered: Notify,
    release: Notify,
    prompts: AtomicUsize,
}

#[async_trait::async_trait]
impl Driver for Held {
    async fn start(&self, _: &Owner, _: &str) -> Result<Binding, String> {
        self.entered.notify_one();
        self.release.notified().await;
        Err("held adapter never opened a session".into())
    }
    async fn prompt(&self, _: &Owner, _: &Binding, _: &str, _: &str) -> Result<(), String> {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        Err("no session was opened".into())
    }
    async fn observe(&self, _: &Owner, _: &Binding, _: u64) -> Result<Observation, String> {
        Err("no session was opened".into())
    }
    async fn permission(
        &self,
        _: &Owner,
        _: &Binding,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), String> {
        Err("no session was opened".into())
    }
    async fn stop(&self, _: &Owner, _: Option<&Binding>, _: &str) -> Result<Stop, String> {
        Ok(Stop {
            run_id: None,
            confirmed: true,
            session_never_started: true,
            detail: "test adapter opened no session".into(),
        })
    }
}

#[tokio::test]
async fn acp_last_dispatch_check_rejects_changed_owner_source_and_workspace_grant()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service =
        ThreadService::with_store(app(vec![])?, &[directory.path().into()], store.clone())?;
    let (source, thread) = worker_thread(&service, &directory, "attempt").await?;
    let control = decisions::control(1, true);
    let driver = Arc::new(Held::default());
    let turn = service
        .start_goal_acp_turn(
            &target(&thread),
            &CallerContext::local(),
            source,
            input("original prompt", "original-turn"),
            control.clone(),
            driver.clone(),
        )
        .await?;
    tokio::time::timeout(Duration::from_secs(3), driver.entered.notified()).await?;
    let owner = Owner {
        server_instance_id: service.inner.instance_id.clone(),
        thread_id: thread.thread_id.clone(),
        turn_id: turn.turn_id.clone(),
    };
    let action = crate::service::worker::acp::Action::Start.key(&turn.turn_id);
    service
        .authorize_acp_dispatch(&owner, &action, control.as_ref())
        .await?;
    let mut obsolete = owner.clone();
    obsolete.server_instance_id = "obsolete-owner".into();
    assert!(
        service
            .authorize_acp_dispatch(&obsolete, &action, control.as_ref())
            .await
            .is_err()
    );
    control.revoked.store(true, Ordering::SeqCst);
    assert!(
        service
            .authorize_acp_dispatch(&owner, &action, control.as_ref())
            .await
            .is_err()
    );
    control.revoked.store(false, Ordering::SeqCst);
    service
        .lock_state()
        .workspace_profiles
        .insert(thread.workspace.clone(), Vec::new());
    assert!(
        service
            .authorize_acp_dispatch(&owner, &action, control.as_ref())
            .await
            .is_err()
    );
    driver.release.notify_one();
    let finished = wait_for(&service, &turn.turn_id, TurnStatus::Failed).await?;
    assert_eq!(finished.status, TurnStatus::Failed);
    assert!(!finished.unknown_effect);
    assert!(
        finished
            .acp_stop
            .as_ref()
            .is_some_and(|stop| stop.confirmed && stop.session_never_started)
    );
    assert_eq!(driver.prompts.load(Ordering::SeqCst), 0);
    let records = store
        .load(&thread.thread_id)
        .await?
        .ok_or("ACP Thread missing")?;
    assert_eq!(records.records.iter().filter(|record| matches!(record, ExecutionRecord::TurnRecord { fact, .. } if matches!(fact.as_ref(), ExecutionRecord::AcpDispatchGate { .. }))).count(), 1);
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn unopened_acp_metadata_cannot_retire_a_live_execution_owner()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let original =
        ThreadService::with_store(app(vec![])?, &[directory.path().into()], store.clone())?;
    let (source, thread) = worker_thread(&original, &directory, "unstarted").await?;
    let restarted =
        ThreadService::with_store(app(vec![])?, &[directory.path().into()], store.clone())?;
    let target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id.clone(),
        server_instance_id: restarted.capabilities().server_instance_id,
    };
    let view = restarted
        .load_thread(&target, &CallerContext::local())
        .await?;
    let report = view
        .recovery
        .as_ref()
        .ok_or("metadata inspection missing")?;
    assert!(report.context_valid && report.terminal_checkpoint);
    assert!(!report.blockers.iter().any(|blocker| matches!(
        blocker,
        crate::thread::RecoveryBlocker::AcpSessionUnconfirmed
    )));
    let driver = Arc::new(Held::default());
    let admission = restarted
        .start_goal_acp_turn(
            &target,
            &CallerContext::local(),
            source,
            input("original unsubmitted task", "first-admission"),
            decisions::control(1, true),
            driver.clone(),
        )
        .await;
    let receipt = restarted
        .find_goal_worker_turn(&target, &CallerContext::local(), "first-admission")
        .await;
    // Even an unexpected admission must be released and joined before an
    // assertion unwinds; this adapter never creates a real process/session.
    driver.release.notify_one();
    restarted.shutdown().await;
    let old_owner = store
        .read_owner(&original.capabilities().server_instance_id)
        .await?;
    original.shutdown().await;
    assert!(admission.is_err_and(|error| error.code == ErrorCode::RecoveryRequired));
    assert!(receipt?.is_none(), "live-owner refusal accepted a Turn");
    assert!(
        old_owner.is_some_and(|owner| owner.stopped_at_ms.is_none()),
        "another runtime retired the live source owner"
    );
    assert_eq!(driver.prompts.load(Ordering::SeqCst), 0);
    Ok(())
}

/// A journal fixture: session metadata is selected, then the prompt response is
/// lost. This adapter never creates an actual process or qualifies a harness.
#[derive(Default)]
struct Reported {
    prompts: AtomicUsize,
}

#[async_trait::async_trait]
impl Driver for Reported {
    async fn start(&self, _: &Owner, _: &str) -> Result<Binding, String> {
        Ok(Binding {
            run_id: "synthetic-run".into(),
            session_id: "synthetic-session".into(),
            agent_id: "test-peer".into(),
            confirmed_model_route: None,
            capabilities: None,
        })
    }
    async fn prompt(&self, _: &Owner, _: &Binding, _: &str, _: &str) -> Result<(), String> {
        self.prompts.fetch_add(1, Ordering::SeqCst);
        Err("prompt response lost; its effects remain unknown".into())
    }
    async fn observe(&self, _: &Owner, _: &Binding, _: u64) -> Result<Observation, String> {
        Err("journal fixture has no complete prompt outcome".into())
    }
    async fn permission(
        &self,
        _: &Owner,
        _: &Binding,
        _: &str,
        _: &str,
        _: &str,
    ) -> Result<(), String> {
        Err("journal fixture offers no permission".into())
    }
    async fn stop(&self, _: &Owner, binding: Option<&Binding>, _: &str) -> Result<Stop, String> {
        Ok(Stop {
            run_id: binding.map(|binding| binding.run_id.clone()),
            confirmed: true,
            session_never_started: false,
            detail: "synthetic controller stopped; submitted effects remain unknown".into(),
        })
    }
}

#[tokio::test]
async fn submitted_acp_prompt_is_not_recovered_as_unopened_metadata()
-> Result<(), Box<dyn std::error::Error>> {
    let directory = tempfile::tempdir()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let original =
        ThreadService::with_store(app(vec![])?, &[directory.path().into()], store.clone())?;
    let (source, thread) = worker_thread(&original, &directory, "submitted").await?;
    let driver = Arc::new(Reported::default());
    let turn = original
        .start_goal_acp_turn(
            &target(&thread),
            &CallerContext::local(),
            source.clone(),
            input("original task", "submitted"),
            decisions::control(1, true),
            driver.clone(),
        )
        .await?;
    let unknown = wait_for(&original, &turn.turn_id, TurnStatus::RecoveryRequired).await?;
    assert!(unknown.unknown_effect && unknown.acp.is_some());
    original.shutdown().await;
    drop(original);
    let restarted = ThreadService::with_store(app(vec![])?, &[directory.path().into()], store)?;
    let target = crate::thread::ThreadTarget {
        thread_id: thread.thread_id,
        server_instance_id: restarted.capabilities().server_instance_id,
    };
    let view = restarted
        .load_thread(&target, &CallerContext::local())
        .await?;
    let report = view
        .recovery
        .as_ref()
        .ok_or("submitted session inspection missing")?;
    assert!(!report.context_valid);
    assert!(report.blockers.iter().any(|blocker| matches!(
        blocker,
        crate::thread::RecoveryBlocker::AcpSessionUnconfirmed
    )));
    let closed = Arc::new(Held::default());
    let admission = restarted
        .start_goal_acp_turn(
            &target,
            &CallerContext::local(),
            source,
            input("must not replay", "must-not-start"),
            decisions::control(1, true),
            closed.clone(),
        )
        .await;
    let receipt = restarted
        .find_goal_worker_turn(&target, &CallerContext::local(), "submitted")
        .await;
    closed.release.notify_one();
    restarted.shutdown().await;
    assert!(admission.is_err_and(|error| error.code == ErrorCode::RecoveryRequired));
    assert_eq!(
        receipt?
            .ok_or("original submitted receipt missing")?
            .turn_id,
        turn.turn_id
    );
    assert_eq!(driver.prompts.load(Ordering::SeqCst), 1);
    assert_eq!(closed.prompts.load(Ordering::SeqCst), 0);
    Ok(())
}
