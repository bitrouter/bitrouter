//! Owner association and receipt recovery never manufacture an execution.

use std::sync::Arc;

use super::support::{app, target, thread_request};
use crate::goal::workflow::owner_thread_key;
use crate::service::{ErrorCode, ThreadService};
use crate::store::{ExecutionStore, MemoryExecutionStore};
use crate::thread::{PermissionProfile, ThreadRequest, ThreadStatus};
use bitrouter_sdk::caller::CallerContext;

#[tokio::test]
async fn goal_owner_creation_uses_one_key_and_preserves_the_original_request()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let store = Arc::new(MemoryExecutionStore::default());
    let service = ThreadService::with_store(app(Vec::new())?, &[], store.clone())?;
    service.register_local_workspace(workspace.path())?;
    let epoch = service.capabilities().server_instance_id;
    let caller = CallerContext::local();
    let key = owner_thread_key("goal-a");
    let request = || {
        let mut request = thread_request(&workspace, &key);
        request.config = request.config.read_only();
        request.permission_profile = PermissionProfile::ReadOnly;
        request
    };
    assert!(
        service
            .find_thread_admission(&epoch, &caller, &key)
            .await?
            .is_none()
    );
    let (first, second) = tokio::join!(
        service.create_goal_owner_thread(&epoch, "goal-a", request()),
        service.create_goal_owner_thread(&epoch, "goal-a", request()),
    );
    let thread = first?;
    assert_eq!(thread.thread_id, second?.thread_id);
    assert_eq!(thread.owned_goal_id.as_deref(), Some("goal-a"));
    assert_eq!(thread.status, ThreadStatus::Idle);
    assert!(thread.active_turn_id.is_none());
    let accepted = store
        .load(&thread.thread_id)
        .await?
        .ok_or("Thread not saved")?;
    let mut different = request();
    different
        .config
        .instructions
        .push_str(" Changed instructions.");
    let error = service
        .create_goal_owner_thread(&epoch, "goal-a", different)
        .await
        .err()
        .ok_or("changed request reused an admission")?;
    assert_eq!(error.code, ErrorCode::Conflict);
    // A generic create cannot erase ownership by reusing the owner key.
    assert!(service.create_thread(&epoch, request()).await.is_err());
    let other = service
        .create_goal_owner_thread(
            &epoch,
            "goal-b",
            ThreadRequest {
                idempotency_key: owner_thread_key("goal-b"),
                ..request()
            },
        )
        .await?;
    assert_ne!(thread.thread_id, other.thread_id);
    service.unload_thread(&target(&thread), &caller).await?;
    let found = service
        .find_thread_admission(&epoch, &caller, &key)
        .await?
        .ok_or("cold receipt missing")?;
    assert_eq!(found.thread_id, thread.thread_id);
    assert_eq!(found.owned_goal_id, thread.owned_goal_id);
    assert!(found.active_turn_id.is_none());
    assert_eq!(
        accepted.version,
        store
            .load(&thread.thread_id)
            .await?
            .ok_or("cold root missing")?
            .version
    );
    assert!(!service.lock_state().threads.contains_key(&thread.thread_id));
    let view = service
        .read_stored_thread_view(&target(&found), &caller)
        .await?;
    assert!(view.latest_turn.is_none());
    service
        .lock_state()
        .workspace_profiles
        .remove(&thread.workspace);
    // Coordination metadata grants no filesystem execution and remains
    // readable after a project's workspace permission is withdrawn.
    assert!(
        service
            .find_thread_admission(&epoch, &caller, &key)
            .await?
            .is_some()
    );
    service.shutdown().await;
    Ok(())
}

#[tokio::test]
async fn ordinary_threads_have_no_goal_and_legacy_snapshots_do_not_invent_one()
-> Result<(), Box<dyn std::error::Error>> {
    let workspace = tempfile::tempdir()?;
    let service = ThreadService::new(app(Vec::new())?, &[workspace.path().into()])?;
    let epoch = service.capabilities().server_instance_id;
    let thread = service
        .create_thread(&epoch, thread_request(&workspace, "ordinary"))
        .await?;
    assert!(thread.owned_goal_id.is_none());
    let value = serde_json::to_value(&thread)?;
    assert!(value.get("owned_goal_id").is_none());
    let legacy: crate::thread::ThreadSnapshot = serde_json::from_value(value)?;
    assert!(legacy.owned_goal_id.is_none());
    assert!(
        service
            .create_goal_owner_thread(&epoch, "goal", thread_request(&workspace, "arbitrary-key"))
            .await
            .is_err()
    );
    assert!(
        service
            .find_thread_admission(&epoch, &CallerContext::anonymous(), "ordinary")
            .await
            .is_err()
    );
    service
        .lock_state()
        .workspace_profiles
        .remove(&thread.workspace);
    service.register_local_workspace(&thread.workspace)?;
    assert!(
        !service
            .lock_state()
            .workspace_profiles
            .contains_key(&thread.workspace)
    );
    assert_eq!(
        service
            .find_thread_admission(&epoch, &CallerContext::local(), "ordinary")
            .await
            .err()
            .ok_or("cold lookup bypassed a withdrawn workspace grant")?
            .code,
        ErrorCode::Unauthorized
    );
    service.shutdown().await;
    Ok(())
}
