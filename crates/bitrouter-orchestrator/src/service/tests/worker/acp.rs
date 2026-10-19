//! Shared dispatch fences with a deliberately held adapter startup. Actual ACP
//! processes are covered separately by the app's Rust protocol fixtures.
use super::*;
use crate::service::worker::acp::{Binding, Driver, Observation, Owner, Stop};
use crate::thread::executor::{AcpConfig, AcpThreadRequest};
use std::time::Duration;
use tokio::sync::Notify;

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
    let root = root(&service, &directory).await?;
    let source = GoalWork {
        owner_thread_id: root.thread_id.clone(),
        goal_id: "goal".into(),
        task_id: "work".into(),
        execution_id: "attempt".into(),
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
