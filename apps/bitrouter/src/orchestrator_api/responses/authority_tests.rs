//! Cached replay must not carry ingress authority across registry contention.

use super::*;
use crate::auth::{db, entities::api_keys, keys};
use crate::orchestrator_api::{Entry, channel::RemotePort, output};
use bitrouter_orchestrator::core::protocol::OwnershipGrant;
use sea_orm::{ActiveModelTrait, EntityTrait, Set};
use serde_json::json;

type TestResult = Result<(), Box<dyn std::error::Error>>;

async fn fixture() -> Result<(ManagedCoreApi, Principal, Create), Box<dyn std::error::Error>> {
    let connection = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&connection).await?;
    db::upsert_user(&connection, "owner").await?;
    let key = keys::generate();
    db::insert_api_key(
        &connection,
        &db::NewApiKey {
            id: "replay-key".into(),
            key_hash: key.hash,
            user_id: "owner".into(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await?;
    let mut headers = http::HeaderMap::new();
    headers.insert(
        http::header::AUTHORIZATION,
        format!("Bearer {}", key.secret).parse()?,
    );
    let principal = Principal::authenticate(&connection, &headers)
        .await
        .map_err(|error| error.0)?;
    let api = ManagedCoreApi::new(
        Arc::new(bitrouter_sdk::App::builder().build()?),
        connection,
        "core".into(),
    );
    let create: Create = serde_json::from_value(json!({
        "model":"model", "input":"task", "multi_agent":{"enabled":true},
        "bitrouter":{"version":1,"execution":"managed","session_id":"session","execution_epoch":1,"operation_id":"replay"}
    }))?;
    let exchange: ResponseExchange = serde_json::from_value(json!({
        "response_id":"response", "operation_id":"replay", "run_id":"run", "model":"model",
        "previous_response_id":null, "created_state_revision":2, "completed_state_revision":3,
        "run_status":"completed", "output":[], "pending":{}
    }))?;
    let (_, progress) = watch::channel(Progress {
        initial: None,
        outcome: Some(Ok(Arc::new(exchange))),
    });
    let limits = Limits::default();
    let grant = OwnershipGrant {
        session_id: "session".into(),
        harness_id: "harness".into(),
        core_instance_id: "core".into(),
        execution_epoch: 1,
    };
    let port = Arc::new(RemotePort::new(
        principal.clone(),
        grant.clone(),
        limits.clone(),
        Arc::new(output::Budget::default()).scope(limits.ephemeral_bytes, None)?,
        Arc::new(output::Budget::default()).scope(limits.unacknowledged_bytes, None)?,
    ));
    api.shared.sessions.lock().await.insert(
        ManagedCoreApi::key(&principal, "session"),
        Entry {
            // This isolated cache test never enters the new-job branch; execution
            // and actual completed-response replay have separate network fixtures.
            session: None,
            port,
            grant,
            limits,
            connected: true,
            ready: true,
            job: Some(Job {
                operation_id: "replay".into(),
                fingerprint: bitrouter_orchestrator::core::checkpoint::sha256(&serde_json::to_vec(
                    &create,
                )?),
                progress,
            }),
            recovery_evidence: Vec::new(),
        },
    );
    Ok((api, principal, create))
}

#[tokio::test]
async fn completed_replay_rechecks_keys_after_the_registry_lock_wait() -> TestResult {
    for expire in [false, true] {
        let (api, principal, create) = fixture().await?;
        let registry = api.shared.sessions.lock().await;
        let mut replay = Box::pin(start(&api, principal, create));
        assert!(futures::poll!(replay.as_mut()).is_pending());
        let mut key: api_keys::ActiveModel = api_keys::Entity::find_by_id("replay-key")
            .one(&api.shared.db)
            .await?
            .ok_or("key")?
            .into();
        if expire {
            key.expires_at = Set(Some(
                (chrono::Utc::now() - chrono::Duration::seconds(1)).to_rfc3339(),
            ));
        } else {
            key.active = Set(0);
        }
        key.update(&api.shared.db).await?;
        drop(registry);
        assert_eq!(
            replay.await.err().ok_or("replay denied")?.0.code,
            ErrorCode::UnauthorizedScope
        );
        api.shared.db.clone().close().await?;
    }
    Ok(())
}

#[tokio::test]
async fn response_rechecks_the_epoch_after_the_registry_lock_wait() -> TestResult {
    let (api, principal, create) = fixture().await?;
    let mut registry = api.shared.sessions.lock().await;
    let mut replay = Box::pin(start(&api, principal.clone(), create));
    assert!(futures::poll!(replay.as_mut()).is_pending());
    registry
        .get_mut(&ManagedCoreApi::key(&principal, "session"))
        .ok_or("session")?
        .grant
        .execution_epoch = 2;
    drop(registry);
    assert_eq!(
        replay.await.err().ok_or("stale replay")?.0.code,
        ErrorCode::StaleEpoch
    );
    api.shared.db.clone().close().await?;
    Ok(())
}
