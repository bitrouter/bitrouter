//! Cross-tenant admission and authority interleavings against real PostgreSQL.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{ExecutionState, TerminalOutcome};
use bitrouter_ai::types::ModelOperation;
use sea_orm::sea_query::Expr;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::Value;
use sha2::Digest;

use crate::auth::{
    db::{NewApiKey, insert_api_key, upsert_user},
    entities::api_keys,
};
use crate::gateway::providers::auth::{credentials::Credential, store::CredentialKey};
use crate::gateway::shared::{
    authority::{Authority, Revision},
    credentials::{CredentialStamp, Credentials},
    crypto::DeploymentKey,
    engine::{Admission, AdmissionResult, Engine, Rejection, TenantLimits, TrajectoryBinding},
    store::{Kind, NewRecord, Store, Stored},
};

mod terminal;

struct Principal {
    id: String,
    tenant: String,
    hash: String,
}

struct Fixture {
    a: Engine,
    b: Engine,
    first: Principal,
    second: Principal,
    pool: String,
    credentials: Credentials,
    slot: CredentialKey,
    stamp: CredentialStamp,
    revision: Revision,
}

pub(super) async fn exercise(first: &Store, second: &Store) -> Result<()> {
    independent_tenants(first, second).await?;
    rate_budgets(first, second).await?;
    expiry_during_lock_wait(first, second).await?;
    finalization_rollback_and_recovery_isolation(first, second).await?;
    publication_and_dispatch(first, second).await?;
    credential_and_dispatch(first, second).await?;
    revocation_and_dispatch(first, second).await?;
    late_terminal_and_recovery(first, second).await?;
    terminal::exercise(first, second).await?;
    key_rotation_and_admission(first, second).await?;
    Ok(())
}

async fn key_rotation_and_admission(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 2).await?;
    let original = match submit(
        &fixture.a,
        &fixture.first,
        fixture.revision,
        Some("rotation"),
    )
    .await?
    {
        AdmissionResult::Accepted(view, _) => view,
        AdmissionResult::Existing(_) => anyhow::bail!("unexpected original duplicate"),
    };
    let authority = Authority::new(second.clone(), DeploymentKey::from_bytes([9; 32])?);
    let head = authority
        .rotate_correlation(fixture.revision.key_generation)
        .await?;
    ensure!(head.config_generation == fixture.revision.config_generation);
    for revision in [
        fixture.revision,
        Revision {
            key_generation: head.key_generation + 1,
            ..head.revision()
        },
    ] {
        let error = submit(&fixture.b, &fixture.first, revision, None)
            .await
            .err()
            .context("mismatched key revision admitted fresh work")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::AuthorityChanged)
        ));
    }
    // A duplicate performs fresh authentication but does not acquire new work
    // or require its original key epoch to remain the active one.
    match submit(
        &fixture.b,
        &fixture.first,
        fixture.revision,
        Some("rotation"),
    )
    .await?
    {
        AdmissionResult::Existing(view) => ensure!(view.execution_id == original.execution_id),
        AdmissionResult::Accepted(..) => anyhow::bail!("rotation replayed existing work"),
    }
    let _ = accepted(&fixture.b, &fixture.first, head.revision()).await?;
    let tenant = row(first, Kind::Tenant, &fixture.first.tenant).await?;
    ensure!(tenant["active"] == 2 && tenant["accepted"] == 2);
    // Rotation alone does not invalidate already accepted provider work.
    let prepared = fixture
        .a
        .prepare_attempt(&fixture.first.hash, &original.execution_id, &fixture.stamp)
        .await?;
    fixture
        .a
        .abandon_prepared(&original.execution_id, &prepared.attempt_id)
        .await?;
    Ok(())
}

impl Fixture {
    async fn new(first: &Store, second: &Store, capacity: u32) -> Result<Self> {
        let authority = Authority::new(first.clone(), DeploymentKey::from_bytes([9; 32])?);
        let revision = authority.head().await?.revision();
        let a = Engine::join(authority.clone(), 30_000).await?;
        let b = Engine::join(
            Authority::new(second.clone(), DeploymentKey::from_bytes([9; 32])?),
            30_000,
        )
        .await?;
        let pool = uuid::Uuid::new_v4().to_string();
        a.set_resource(&pool, None, capacity).await?;
        let first_principal = principal(first, None, None).await?;
        let second_principal = principal(first, None, None).await?;
        let limits = TenantLimits {
            concurrent_executions: 8,
            requests_per_minute: 100,
            resource_pools: [pool.clone()].into(),
        };
        a.set_tenant(&first_principal.tenant, None, &limits).await?;
        b.set_tenant(&second_principal.tenant, None, &limits)
            .await?;
        let credentials = Credentials::new(authority);
        let slot = CredentialKey {
            provider: pool.clone(),
            account: "default".into(),
        };
        let stamp = credentials
            .import(
                &slot,
                &pool,
                None,
                &Credential::api_key("fixture-account-secret"),
            )
            .await?;
        Ok(Self {
            a,
            b,
            first: first_principal,
            second: second_principal,
            pool,
            credentials,
            slot,
            stamp,
            revision,
        })
    }
}

async fn principal(store: &Store, tenant: Option<&str>, rpm: Option<i64>) -> Result<Principal> {
    let tenant = tenant
        .map(str::to_owned)
        .unwrap_or_else(|| uuid::Uuid::new_v4().to_string());
    let id = uuid::Uuid::new_v4().to_string();
    let hash = crate::auth::keys::hash_key(&uuid::Uuid::new_v4().to_string());
    upsert_user(store.database(), &tenant).await?;
    insert_api_key(
        store.database(),
        &NewApiKey {
            id: id.clone(),
            key_hash: hash.clone(),
            user_id: tenant.clone(),
            spend_limit_micro_usd: None,
            rpm_limit: rpm,
            policy_id: None,
        },
    )
    .await?;
    Ok(Principal { id, tenant, hash })
}

async fn submit(
    engine: &Engine,
    principal: &Principal,
    revision: Revision,
    key: Option<&str>,
) -> Result<AdmissionResult> {
    engine
        .admit(&Admission {
            key_hash: &principal.hash,
            correlation_id: "fixture-correlation",
            idempotency_key: key,
            fingerprint: &"d".repeat(64),
            operation: ModelOperation::Generation,
            revision,
        })
        .await
}

async fn accepted(engine: &Engine, principal: &Principal, revision: Revision) -> Result<String> {
    match submit(engine, principal, revision, None).await? {
        AdmissionResult::Accepted(execution, _) => Ok(execution.execution_id),
        AdmissionResult::Existing(_) => {
            anyhow::bail!("new fixture request unexpectedly deduplicated")
        }
    }
}

async fn clock(store: &Store) -> Result<i64> {
    let tx = store.begin().await?;
    let now = Store::now(&tx).await?;
    tx.commit().await?;
    Ok(now)
}

async fn row(store: &Store, kind: Kind, key: &str) -> Result<Value> {
    Ok(store
        .get::<Value>(kind, key)
        .await?
        .context("fixture row missing")?
        .value)
}

async fn independent_tenants(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 1).await?;
    let alias = CredentialKey {
        provider: format!("alias-{}", fixture.pool),
        account: "same-account".into(),
    };
    let alias_stamp = fixture
        .credentials
        .import(
            &alias,
            &fixture.pool,
            None,
            &Credential::api_key("fixture-account-secret"),
        )
        .await?;
    let one = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    let two = accepted(&fixture.b, &fixture.second, fixture.revision).await?;
    let (a, b) = tokio::join!(
        fixture
            .a
            .prepare_attempt(&fixture.first.hash, &one, &fixture.stamp),
        fixture
            .b
            .prepare_attempt(&fixture.second.hash, &two, &alias_stamp)
    );
    ensure!(
        a.is_ok() != b.is_ok(),
        "different tenants acquired the same sole provider permit"
    );
    let error = a
        .err()
        .or_else(|| b.err())
        .context("resource rejection missing")?;
    ensure!(matches!(
        error.downcast_ref::<Rejection>(),
        Some(Rejection::Quota)
    ));
    fixture
        .a
        .finish(
            &one,
            &TerminalOutcome::Failed {
                code: "fixture_abort".into(),
            },
        )
        .await?;
    fixture
        .b
        .finish(
            &two,
            &TerminalOutcome::Failed {
                code: "fixture_abort".into(),
            },
        )
        .await?;
    ensure!(row(first, Kind::Resource, &fixture.pool).await?["active"] == 0);
    Ok(())
}

async fn rate_budgets(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 1).await?;
    let limited = principal(first, Some(&fixture.first.tenant), Some(1)).await?;
    fixture
        .a
        .set_tenant(
            &fixture.first.tenant,
            Some(1),
            &TenantLimits {
                concurrent_executions: 8,
                requests_per_minute: 2,
                resource_pools: [fixture.pool.clone()].into(),
            },
        )
        .await?;
    // Keep the short quota campaign in one database-clock minute.
    let remaining = 60_000 - clock(first).await?.rem_euclid(60_000);
    if remaining < 2_000 {
        tokio::time::sleep(Duration::from_millis(u64::try_from(remaining)? + 20)).await;
    }
    let admitted = match submit(&fixture.a, &limited, fixture.revision, Some("once")).await? {
        AdmissionResult::Accepted(execution, _) => execution.execution_id,
        _ => anyhow::bail!("first quota fixture deduplicated"),
    };
    fixture
        .a
        .finish(
            &admitted,
            &TerminalOutcome::Failed {
                code: "predispatch_rejection".into(),
            },
        )
        .await?;
    ensure!(matches!(
        submit(&fixture.b, &limited, fixture.revision, Some("once")).await?,
        AdmissionResult::Existing(_)
    ));
    ensure!(matches!(
        accepted(&fixture.b, &limited, fixture.revision)
            .await
            .err()
            .and_then(|error| error.downcast::<Rejection>().ok()),
        Some(Rejection::Quota)
    ));
    ensure!(
        row(first, Kind::Tenant, &limited.tenant).await?["accepted"] == 1,
        "principal rejection or duplicate charged tenant rate"
    );
    ensure!(row(first, Kind::Principal, &limited.id).await?["accepted"] == 1);
    let second = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    fixture
        .a
        .finish(
            &second,
            &TerminalOutcome::Failed {
                code: "predispatch_rejection".into(),
            },
        )
        .await?;
    ensure!(
        accepted(&fixture.b, &fixture.first, fixture.revision)
            .await
            .is_err(),
        "pre-dispatch failure refunded accepted rate charge"
    );
    let tenant = row(first, Kind::Tenant, &limited.tenant).await?;
    ensure!(
        tenant["active"] == 0 && tenant["accepted"] == 2,
        "rejected admission changed reservations or rate"
    );
    Ok(())
}

async fn expiry_during_lock_wait(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 1).await?;
    let expiry_ms = clock(first).await? + 800;
    let expiry = chrono::DateTime::from_timestamp_millis(expiry_ms)
        .context("fixture expiry out of range")?
        .to_rfc3339();
    api_keys::Entity::update_many()
        .col_expr(api_keys::Column::ExpiresAt, Expr::value(expiry))
        .filter(api_keys::Column::Id.eq(&fixture.first.id))
        .exec(first.database())
        .await?;
    let execution = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    let attempt = fixture
        .a
        .prepare_attempt(&fixture.first.hash, &execution, &fixture.stamp)
        .await?;
    let blocker = second.begin().await?;
    let _: Stored<Value> = second.lock(&blocker, Kind::Resource, &fixture.pool).await?;
    let owner = fixture.a.clone();
    let hash = fixture.first.hash.clone();
    let id = execution.clone();
    let attempt_id = attempt.attempt_id.clone();
    let mut pending = tokio::spawn(async move { owner.dispatch(&hash, &id, &attempt_id).await });
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), &mut pending)
            .await
            .is_err()
    );
    let remaining = expiry_ms - clock(first).await?;
    if remaining > 0 {
        tokio::time::sleep(Duration::from_millis(u64::try_from(remaining)? + 20)).await;
    }
    blocker.commit().await?;
    let error = pending
        .await?
        .err()
        .context("expired principal obtained dispatch intent after lock wait")?;
    ensure!(matches!(
        error.downcast_ref::<Rejection>(),
        Some(Rejection::Unauthorized)
    ));
    ensure!(
        row(first, Kind::Attempt, &attempt.attempt_id).await?["view"]["state"]["status"]
            == "prepared"
    );
    fixture
        .a
        .finish(
            &execution,
            &TerminalOutcome::Failed {
                code: "principal_expired".into(),
            },
        )
        .await?;
    Ok(())
}

async fn finalization_rollback_and_recovery_isolation(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 2).await?;
    let authority = Authority::new(first.clone(), DeploymentKey::from_bytes([9; 32])?);
    let owner = Engine::join(authority, 1_000).await?;
    let bad = accepted(&owner, &fixture.first, fixture.revision).await?;
    let good = accepted(&owner, &fixture.second, fixture.revision).await?;
    let binding = TrajectoryBinding {
        owner_user_id: fixture.first.tenant.clone(),
        episode_id: uuid::Uuid::new_v4().to_string(),
        request_id: "fixture-request".into(),
        correlation_epoch: "fixture-epoch".into(),
    };
    let tx = first.begin().await?;
    owner.bind_trajectory(&tx, &bad, &binding).await?;
    tx.commit().await?;
    let attempt = owner
        .prepare_attempt(&fixture.first.hash, &bad, &fixture.stamp)
        .await?;
    let fence_key = hex::encode(sha2::Sha256::digest(serde_json::to_vec(&(
        "gateway-episode-fence-v1",
        &binding.owner_user_id,
        &binding.episode_id,
    ))?));
    let event_id = format!("gateway-terminal-{bad}");
    // Inject a durable collision after all earlier transaction writes would run.
    // Keep it outside the delivery queue; this is a private fixture-only fault.
    let tx = first.begin().await?;
    first
        .create(
            &tx,
            NewRecord {
                kind: Kind::Outbox,
                key: &event_id,
                status: "fixture",
                due_at_ms: None,
                now_ms: Store::now(&tx).await?,
                value: &serde_json::json!({"fault": "collision"}),
            },
        )
        .await?;
    tx.commit().await?;
    let before = row(first, Kind::Execution, &bad).await?;
    ensure!(
        owner
            .finish(
                &bad,
                &TerminalOutcome::Failed {
                    code: "fixture_failure".into()
                }
            )
            .await
            .is_err()
    );
    ensure!(
        row(first, Kind::Execution, &bad).await? == before,
        "failed terminal transaction changed execution"
    );
    ensure!(row(first, Kind::Resource, &fixture.pool).await?["active"] == 1);
    ensure!(row(first, Kind::Tenant, &fixture.first.tenant).await?["active"] == 1);
    ensure!(
        row(first, Kind::Attempt, &attempt.attempt_id).await?["view"]["state"]["status"]
            == "prepared"
    );
    ensure!(row(first, Kind::EpisodeFence, &fence_key).await?["pending"] == serde_json::json!([]));
    ensure!(
        row(first, Kind::Outbox, &event_id).await? == serde_json::json!({"fault": "collision"})
    );
    tokio::time::sleep(Duration::from_millis(1_050)).await;
    // Force the failing row first, regardless of hashed IDs or other fixtures.
    // A batch of one must defer it and let the next pass reach the other tenant.
    let tx = first.begin().await?;
    for (id, due, status) in [(&bad, 0, "running"), (&good, 1, "accepted")] {
        let mut record: Stored<Value> = first.lock(&tx, Kind::Execution, id).await?;
        first
            .save(&tx, &mut record, status, Some(due), Store::now(&tx).await?)
            .await?;
    }
    tx.commit().await?;
    let report = fixture.b.reconcile(1).await?;
    ensure!(report.visited == 1 && report.deferred == 1);
    let deferred = first
        .get::<Value>(Kind::Execution, &bad)
        .await?
        .context("deferred execution missing")?;
    let now = clock(first).await?;
    ensure!(deferred.due_at_ms().is_some_and(|due| due > now));
    let report = fixture.b.reconcile(1).await?;
    ensure!(report.visited == 1 && report.deferred == 0);
    ensure!(
        matches!(
            fixture.b.query(&fixture.second.hash, &good).await?.state,
            ExecutionState::Terminal { .. }
        ),
        "failed record starved another tenant's recovery"
    );
    ensure!(matches!(
        fixture.b.query(&fixture.first.hash, &bad).await?.state,
        ExecutionState::Running
    ));
    Ok(())
}

async fn publish_generation(store: &Store) -> Result<u64> {
    store
        .retry(|| async {
            let tx = store.begin().await?;
            let mut head: Stored<Value> = store.lock(&tx, Kind::Authority, "head").await?;
            let generation = head.value["config_generation"]
                .as_u64()
                .context("generation missing")?
                + 1;
            head.value["config_generation"] = generation.into();
            store
                .save(&tx, &mut head, "active", None, Store::now(&tx).await?)
                .await?;
            tx.commit().await?;
            Ok(generation)
        })
        .await
}

async fn publication_and_dispatch(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 2).await?;
    let one = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    let two = accepted(&fixture.b, &fixture.second, fixture.revision).await?;
    let first_attempt = fixture
        .a
        .prepare_attempt(&fixture.first.hash, &one, &fixture.stamp)
        .await?;
    let second_attempt = fixture
        .b
        .prepare_attempt(&fixture.second.hash, &two, &fixture.stamp)
        .await?;
    let blocker = second.begin().await?;
    let _: Stored<Value> = second.lock(&blocker, Kind::Resource, &fixture.pool).await?;
    let owner = fixture.a.clone();
    let hash = fixture.first.hash.clone();
    let id = one.clone();
    let attempt_id = first_attempt.attempt_id.clone();
    let mut dispatch = tokio::spawn(async move { owner.dispatch(&hash, &id, &attempt_id).await });
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), &mut dispatch)
            .await
            .is_err()
    );
    let publisher = second.clone();
    let mut publication = tokio::spawn(async move { publish_generation(&publisher).await });
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), &mut publication)
            .await
            .is_err(),
        "publication crossed in-flight dispatch fence"
    );
    blocker.commit().await?;
    dispatch.await??;
    let generation = publication.await??;
    ensure!(generation == fixture.revision.config_generation + 1);
    ensure!(
        row(first, Kind::Attempt, &first_attempt.attempt_id).await?["view"]["state"]["status"]
            == "dispatched"
    );
    ensure!(
        fixture
            .b
            .dispatch(&fixture.second.hash, &two, &second_attempt.attempt_id)
            .await
            .is_err(),
        "stale generation authorized dispatch after publication"
    );
    fixture
        .b
        .finish(
            &two,
            &TerminalOutcome::Failed {
                code: "authority_changed".into(),
            },
        )
        .await?;
    fixture
        .a
        .mark_unknown(&one, &first_attempt.attempt_id)
        .await?;
    Ok(())
}

async fn credential_and_dispatch(first: &Store, second: &Store) -> Result<()> {
    for replacement_first in [true, false] {
        let fixture = Fixture::new(first, second, 1).await?;
        let id = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
        let attempt = fixture
            .a
            .prepare_attempt(&fixture.first.hash, &id, &fixture.stamp)
            .await?;
        let blocker = second.begin().await?;
        let _: Stored<Value> = second.lock(&blocker, Kind::Resource, &fixture.pool).await?;
        let owner = fixture.a.clone();
        let hash = fixture.first.hash.clone();
        let execution_id = id.clone();
        let attempt_id = attempt.attempt_id.clone();
        let send = async move { owner.dispatch(&hash, &execution_id, &attempt_id).await };
        let credentials = fixture.credentials.clone();
        let slot = fixture.slot.clone();
        let pool = fixture.pool.clone();
        let replace = async move {
            credentials
                .import(
                    &slot,
                    &pool,
                    Some(1),
                    &Credential::api_key("fixture-new-token"),
                )
                .await
        };
        let (dispatch, replacement) = if replacement_first {
            let mut replacement = tokio::spawn(replace);
            ensure!(
                tokio::time::timeout(Duration::from_millis(100), &mut replacement)
                    .await
                    .is_err()
            );
            (tokio::spawn(send), replacement)
        } else {
            let mut dispatch = tokio::spawn(send);
            ensure!(
                tokio::time::timeout(Duration::from_millis(100), &mut dispatch)
                    .await
                    .is_err()
            );
            (dispatch, tokio::spawn(replace))
        };
        tokio::time::sleep(Duration::from_millis(30)).await;
        blocker.commit().await?;
        let dispatched = dispatch.await?;
        replacement.await??;
        if replacement_first {
            ensure!(
                dispatched.is_err(),
                "credential replacement committed before stale dispatch"
            );
            fixture
                .a
                .finish(
                    &id,
                    &TerminalOutcome::Failed {
                        code: "credential_changed".into(),
                    },
                )
                .await?;
        } else {
            dispatched?;
            fixture.a.mark_unknown(&id, &attempt.attempt_id).await?;
        }
    }
    Ok(())
}

async fn revocation_and_dispatch(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, second, 1).await?;
    let id = accepted(&fixture.a, &fixture.first, fixture.revision).await?;
    let attempt = fixture
        .a
        .prepare_attempt(&fixture.first.hash, &id, &fixture.stamp)
        .await?;
    let revoke = second.begin().await?;
    api_keys::Entity::update_many()
        .col_expr(api_keys::Column::Active, Expr::value(0))
        .filter(api_keys::Column::Id.eq(&fixture.first.id))
        .exec(&revoke)
        .await?;
    let owner = fixture.a.clone();
    let hash = fixture.first.hash.clone();
    let execution_id = id.clone();
    let attempt_id = attempt.attempt_id.clone();
    let mut dispatch =
        tokio::spawn(async move { owner.dispatch(&hash, &execution_id, &attempt_id).await });
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), &mut dispatch)
            .await
            .is_err()
    );
    revoke.commit().await?;
    ensure!(matches!(
        dispatch
            .await?
            .err()
            .and_then(|error| error.downcast::<Rejection>().ok()),
        Some(Rejection::Unauthorized)
    ));
    ensure!(
        row(first, Kind::Attempt, &attempt.attempt_id).await?["view"]["state"]["status"]
            == "prepared"
    );
    fixture
        .a
        .finish(
            &id,
            &TerminalOutcome::Failed {
                code: "revoked".into(),
            },
        )
        .await?;
    Ok(())
}

async fn late_terminal_and_recovery(first: &Store, second: &Store) -> Result<()> {
    use bitrouter_ai::gateway::execution::{ExecutionUsage, TerminalObservation};
    use bitrouter_ai::types::UsageOrigin;
    let fixture = Fixture::new(first, second, 1).await?;
    let owner = Engine::join(
        Authority::new(first.clone(), DeploymentKey::from_bytes([9; 32])?),
        1_000,
    )
    .await?;
    let id = accepted(&owner, &fixture.first, fixture.revision).await?;
    let attempt = owner
        .prepare_attempt(&fixture.first.hash, &id, &fixture.stamp)
        .await?;
    owner
        .dispatch(&fixture.first.hash, &id, &attempt.attempt_id)
        .await?;
    let terminal = TerminalObservation {
        outcome: TerminalOutcome::Succeeded,
        recorded_at_ms: 0,
        usage: ExecutionUsage {
            origin: UsageOrigin::ProviderReported,
            prompt_tokens: Some(3),
            completion_tokens: Some(2),
            reasoning_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            total_tokens: Some(5),
            cost_micro_usd: Some(7),
        },
    };
    tokio::time::sleep(Duration::from_millis(1_050)).await;
    let (late, recovery) = tokio::join!(
        owner.record_terminal(&id, &attempt.attempt_id, &terminal),
        fixture.b.reconcile(256)
    );
    late?;
    recovery?;
    fixture.b.reconcile(256).await?;
    let result = fixture.b.query(&fixture.first.hash, &id).await?;
    ensure!(
        matches!(result.state, ExecutionState::Terminal {ref result} if result.usage.cost_micro_usd == Some(7))
    );
    ensure!(row(first, Kind::Resource, &fixture.pool).await?["active"] == 0);
    ensure!(row(first, Kind::Tenant, &fixture.first.tenant).await?["active"] == 0);
    owner
        .record_terminal(&id, &attempt.attempt_id, &terminal)
        .await?;
    ensure!(owner.finish(&id, &TerminalOutcome::Succeeded).await? == result);
    Ok(())
}
