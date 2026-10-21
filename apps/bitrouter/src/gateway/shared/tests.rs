//! Real PostgreSQL fixtures. The opt-in URL must name a dedicated test database.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde::{Deserialize, Serialize};
use sha2::Digest;

use super::authority::Authority;
use super::crypto::DeploymentKey;
use super::engine::{Admission, AdmissionResult, Engine, Rejection, TenantLimits};
use super::schema;
use super::store::{self, Kind, NewRecord, Store, Stored};

mod admitted;
mod dispatched;
mod evaluation;
mod metering;
mod publication;
mod races;
mod refresh;
mod trajectory;

#[derive(Debug, Serialize, Deserialize, PartialEq, Eq)]
struct Counter {
    value: u64,
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL for a dedicated PostgreSQL test database"]
async fn postgres_foundation() -> Result<()> {
    tokio::time::timeout(Duration::from_secs(60), foundation())
        .await
        .context("shared foundation regression deadline exceeded")?
}

async fn foundation() -> Result<()> {
    let url = std::env::var("BITROUTER_TEST_DATABASE_URL")
        .context("set BITROUTER_TEST_DATABASE_URL; no database fallback is used")?;
    let first_db = store::connect(&url, 4, Duration::from_secs(3)).await?;
    let second_db = store::connect(&url, 4, Duration::from_secs(3)).await?;
    let namespace = format!("test-{}", uuid::Uuid::new_v4());
    let first = Store::new(first_db.clone(), &namespace, Duration::from_secs(3))?;
    let second = Store::new(second_db.clone(), &namespace, Duration::from_secs(3))?;
    let result = async {
        let (a, b) = tokio::join!(schema::initialize(&first), schema::initialize(&second));
        a?;
        b?;
        namespace_and_rollback(&first, &second).await?;
        key_authority_and_publication_fence(&first, &second).await?;
        transaction_timeout(&url, &first).await?;
        pooled_read_timeout(&url, &first).await?;
        connector_trace_is_redacted(&url).await?;
        shared_admission(&first, &second).await?;
        shared_recovery(&first, &second).await?;
        races::exercise(&first, &second).await?;
        refresh::exercise(&first, &second).await?;
        publication::exercise(&first, &second).await?;
        super::authentication::tests::exercise(&first, &second).await?;
        admitted::exercise(&first, &second).await?;
        // Give the executor campaign its own polling root, as production HTTP
        // connections do, rather than nesting it inside every foundation frame.
        let mut executors = tokio::task::JoinSet::new();
        let inputs = (first.clone(), second.clone());
        executors.spawn(async move { dispatched::exercise(&inputs.0, &inputs.1).await });
        executors
            .join_next()
            .await
            .context("missing executor campaign")???;
        Box::pin(super::ownership::tests::exercise(&first, &second)).await?;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    let first_closed = first_db.close().await;
    let second_closed = second_db.close().await;
    result?;
    first_closed?;
    second_closed?;
    Ok(())
}

async fn shared_admission(first: &Store, second: &Store) -> Result<()> {
    use crate::auth::db::{NewApiKey, insert_api_key, upsert_user};
    use crate::auth::entities::api_keys;
    use bitrouter_ai::types::ModelOperation;
    use sea_orm::sea_query::Expr;
    use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};

    crate::db::run_migrations(first.database()).await?;
    let tenant = format!("tenant-{}", uuid::Uuid::new_v4());
    let other_tenant = format!("other-{}", uuid::Uuid::new_v4());
    let principal = format!("principal-{}", uuid::Uuid::new_v4());
    let key_hash = crate::auth::keys::hash_key(&format!("fixture-{}", uuid::Uuid::new_v4()));
    let other_hash =
        crate::auth::keys::hash_key(&format!("other-fixture-{}", uuid::Uuid::new_v4()));
    for (owner, id, hash) in [
        (&tenant, principal.clone(), &key_hash),
        (
            &other_tenant,
            format!("principal-{}", uuid::Uuid::new_v4()),
            &other_hash,
        ),
    ] {
        upsert_user(first.database(), owner).await?;
        insert_api_key(
            first.database(),
            &NewApiKey {
                id,
                key_hash: hash.clone(),
                user_id: owner.clone(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
    }
    let revision = Authority::new(first.clone(), DeploymentKey::fixture()?)
        .head()
        .await?
        .revision();
    let a = Engine::join(
        Authority::new(first.clone(), DeploymentKey::fixture()?),
        30_000,
    )
    .await?;
    let b = Engine::join(
        Authority::new(second.clone(), DeploymentKey::fixture()?),
        30_000,
    )
    .await?;
    let limits = TenantLimits {
        concurrent_executions: 2,
        requests_per_minute: 100,
        resource_pools: ["pool-a".into()].into(),
    };
    ensure!(a.set_tenant(&tenant, None, &limits).await? == 1);
    b.set_tenant(&other_tenant, None, &limits).await?;
    let fingerprint = "a".repeat(64);
    let submission = Admission {
        requirements: crate::gateway::shared::capture::Requirements::fixture(),
        key_hash: &key_hash,
        correlation_id: "caller-correlation",
        idempotency_key: Some("same-key"),
        fingerprint: &fingerprint,
        operation: ModelOperation::Generation,
        revision,
    };
    let (left, right) = tokio::join!(a.admit(&submission), b.admit(&submission));
    let left = left?;
    let right = right?;
    let (execution_id, first_owner, second_owner) = match (left, right) {
        (AdmissionResult::Accepted(first, _), AdmissionResult::Existing(second)) => {
            ensure!(first.execution_id == second.execution_id);
            (first.execution_id, &a, &b)
        }
        (AdmissionResult::Existing(first), AdmissionResult::Accepted(second, _)) => {
            ensure!(first.execution_id == second.execution_id);
            (first.execution_id, &b, &a)
        }
        _ => anyhow::bail!("concurrent duplicate did not produce exactly one new execution"),
    };
    ensure!(b.query(&key_hash, &execution_id).await?.correlation_id == "caller-correlation");
    ensure!(matches!(
        b.query(&other_hash, &execution_id)
            .await
            .err()
            .and_then(|error| error.downcast::<Rejection>().ok()),
        Some(Rejection::NotFound)
    ));
    let conflict_hash = "b".repeat(64);
    let conflict = Admission {
        requirements: submission.requirements.clone(),
        fingerprint: &conflict_hash,
        ..submission
    };
    ensure!(matches!(
        a.admit(&conflict)
            .await
            .err()
            .and_then(|error| error.downcast::<Rejection>().ok()),
        Some(Rejection::IdempotencyConflict)
    ));
    let fresh = Admission {
        requirements: submission.requirements.clone(),
        idempotency_key: None,
        ..submission
    };
    let second_execution = match second_owner.admit(&fresh).await? {
        AdmissionResult::Accepted(execution, _) => execution.execution_id,
        AdmissionResult::Existing(_) => anyhow::bail!("fresh request unexpectedly deduplicated"),
    };
    ensure!(matches!(
        a.admit(&fresh)
            .await
            .err()
            .and_then(|error| error.downcast::<Rejection>().ok()),
        Some(Rejection::Quota)
    ));
    shared_attempts(
        first,
        first_owner,
        second_owner,
        &key_hash,
        &execution_id,
        &second_execution,
    )
    .await?;
    let lowered = TenantLimits {
        concurrent_executions: 0,
        ..limits
    };
    ensure!(b.set_tenant(&tenant, Some(1), &lowered).await? == 2);
    ensure!(a.set_tenant(&tenant, Some(1), &lowered).await.is_err());
    ensure!(
        matches!(b.admit(&submission).await?, AdmissionResult::Existing(_)),
        "duplicate must not acquire capacity again"
    );
    api_keys::Entity::update_many()
        .col_expr(api_keys::Column::Active, Expr::value(0))
        .filter(api_keys::Column::Id.eq(&principal))
        .exec(first.database())
        .await?;
    ensure!(
        matches!(
            b.admit(&submission)
                .await
                .err()
                .and_then(|error| error.downcast::<Rejection>().ok()),
            Some(Rejection::Unauthorized)
        ),
        "duplicate must reauthenticate revoked keys"
    );
    a.heartbeat().await?;
    a.drain().await?;
    let other = Admission {
        requirements: crate::gateway::shared::capture::Requirements::fixture(),
        key_hash: &other_hash,
        ..fresh
    };
    ensure!(matches!(
        a.admit(&other)
            .await
            .err()
            .and_then(|error| error.downcast::<Rejection>().ok()),
        Some(Rejection::Unavailable)
    ));
    ensure!(matches!(
        b.admit(&other).await?,
        AdmissionResult::Accepted(_, _)
    ));
    Ok(())
}

async fn shared_attempts(
    store: &Store,
    a: &Engine,
    b: &Engine,
    key_hash: &str,
    first_id: &str,
    second_id: &str,
) -> Result<()> {
    use super::credentials::Credentials;
    use crate::gateway::providers::auth::{credentials::Credential, store::CredentialKey};
    use bitrouter_ai::gateway::execution::{
        AttemptState, ExecutionUsage, TerminalObservation, TerminalOutcome,
    };
    use bitrouter_ai::types::UsageOrigin;
    a.set_resource("pool-a", None, 1).await?;
    let credentials = Credentials::new(Authority::new(store.clone(), DeploymentKey::fixture()?));
    let slot = CredentialKey {
        provider: "fixture-provider".into(),
        account: "default".into(),
    };
    let alias = CredentialKey {
        provider: "fixture-alias".into(),
        account: "same-account".into(),
    };
    let secret = Credential::api_key("fixture-secret-first");
    let original = credentials.import(&slot, "pool-a", None, &secret).await?;
    let aliased = credentials.import(&alias, "pool-a", None, &secret).await?;
    ensure!(credentials.load(&slot).await? == (original.clone(), secret));
    let (left, right) = tokio::join!(
        a.prepare_attempt(key_hash, first_id, &original),
        b.prepare_attempt(key_hash, second_id, &aliased)
    );
    let (winner, winner_id, attempt, loser, loser_id, loser_slot, loser_stamp) = match (left, right)
    {
        (Ok(attempt), Err(error)) => {
            ensure!(matches!(
                error.downcast_ref::<Rejection>(),
                Some(Rejection::Quota)
            ));
            (a, first_id, attempt, b, second_id, &alias, aliased)
        }
        (Err(error), Ok(attempt)) => {
            ensure!(matches!(
                error.downcast_ref::<Rejection>(),
                Some(Rejection::Quota)
            ));
            (b, second_id, attempt, a, first_id, &slot, original)
        }
        _ => anyhow::bail!("account aliases did not enforce one shared permit"),
    };
    winner
        .dispatch(key_hash, winner_id, &attempt.attempt_id)
        .await?;
    ensure!(
        winner
            .dispatch(key_hash, winner_id, &attempt.attempt_id)
            .await
            .is_err(),
        "repeated dispatch intent authorized another send"
    );
    ensure!(
        winner
            .abandon_prepared(winner_id, &attempt.attempt_id)
            .await
            .is_err(),
        "dispatched work was treated as safely abandoned"
    );
    ensure!(
        loser
            .prepare_attempt(key_hash, loser_id, &loser_stamp)
            .await
            .is_err(),
        "in-flight provider permit was released early"
    );
    let terminal = TerminalObservation {
        outcome: TerminalOutcome::Succeeded,
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
        recorded_at_ms: 0,
    };
    let (first, repeated) = tokio::join!(
        winner.record_terminal(winner_id, &attempt.attempt_id, &terminal),
        winner.record_terminal(winner_id, &attempt.attempt_id, &terminal)
    );
    first?;
    repeated?;
    let contradictory = TerminalObservation {
        outcome: TerminalOutcome::Failed {
            code: "conflict".into(),
        },
        ..terminal
    };
    ensure!(
        winner
            .record_terminal(winner_id, &attempt.attempt_id, &contradictory)
            .await
            .is_err()
    );
    ensure!(matches!(
        winner.query(key_hash, winner_id).await?.attempts[0].state,
        AttemptState::Terminal { .. }
    ));
    let pending = loser
        .prepare_attempt(key_hash, loser_id, &loser_stamp)
        .await?;
    let replacement = credentials
        .import(
            loser_slot,
            "pool-a",
            Some(1),
            &Credential::api_key("fixture-secret-replaced"),
        )
        .await?;
    ensure!(
        loser
            .dispatch(key_hash, loser_id, &pending.attempt_id)
            .await
            .is_err(),
        "stale credential crossed dispatch fence"
    );
    loser
        .abandon_prepared(loser_id, &pending.attempt_id)
        .await?;
    loser
        .abandon_prepared(loser_id, &pending.attempt_id)
        .await?;
    let next = loser
        .prepare_attempt(key_hash, loser_id, &replacement)
        .await?;
    ensure!(next.credential_revision == 2);
    // A competing reservation must still fail: repeated terminal/abort writes
    // must not decrement the resource occupancy twice.
    ensure!(
        winner
            .prepare_attempt(key_hash, winner_id, &replacement)
            .await
            .is_err()
    );
    loser.abandon_prepared(loser_id, &next.attempt_id).await?;
    Ok(())
}

async fn shared_recovery(first: &Store, second: &Store) -> Result<()> {
    use super::credentials::Credentials;
    use super::engine::TrajectoryBinding;
    use crate::auth::db::{NewApiKey, insert_api_key, upsert_user};
    use crate::gateway::providers::auth::{credentials::Credential, store::CredentialKey};
    use bitrouter_ai::gateway::execution::{
        AttemptState, ExecutionState, ExecutionUsage, TerminalObservation, TerminalOutcome,
    };
    use bitrouter_ai::types::{ModelOperation, UsageOrigin};
    let authority = Authority::new(first.clone(), DeploymentKey::fixture()?);
    let revision = authority.head().await?.revision();
    let owner = Engine::join(authority.clone(), 1_000).await?;
    let reconciler = Engine::join(
        Authority::new(second.clone(), DeploymentKey::fixture()?),
        30_000,
    )
    .await?;
    let tenant = format!("recovery-{}", uuid::Uuid::new_v4());
    let key_hash = crate::auth::keys::hash_key(&format!("fixture-{}", uuid::Uuid::new_v4()));
    upsert_user(first.database(), &tenant).await?;
    insert_api_key(
        first.database(),
        &NewApiKey {
            id: uuid::Uuid::new_v4().to_string(),
            key_hash: key_hash.clone(),
            user_id: tenant.clone(),
            spend_limit_micro_usd: None,
            rpm_limit: None,
            policy_id: None,
        },
    )
    .await?;
    owner
        .set_tenant(
            &tenant,
            None,
            &TenantLimits {
                concurrent_executions: 4,
                requests_per_minute: 100,
                resource_pools: ["recovery-pool".into()].into(),
            },
        )
        .await?;
    owner.set_resource("recovery-pool", None, 2).await?;
    let credentials = Credentials::new(authority);
    let stamp = credentials
        .import(
            &CredentialKey {
                provider: "recovery".into(),
                account: "default".into(),
            },
            "recovery-pool",
            None,
            &Credential::api_key("fixture-recovery-token"),
        )
        .await?;
    let fingerprint = "c".repeat(64);
    let mut executions = Vec::new();
    for _ in 0..4 {
        match owner
            .admit(&Admission {
                requirements: crate::gateway::shared::capture::Requirements::fixture(),
                key_hash: &key_hash,
                correlation_id: "same-external-id",
                idempotency_key: None,
                fingerprint: &fingerprint,
                operation: ModelOperation::Generation,
                revision,
            })
            .await?
        {
            AdmissionResult::Accepted(execution, _) => executions.push(execution.execution_id),
            AdmissionResult::Existing(_) => {
                anyhow::bail!("external correlation ID unexpectedly deduplicated a request")
            }
        }
    }
    let binding = TrajectoryBinding {
        owner_user_id: tenant.clone(),
        episode_id: uuid::Uuid::new_v4().to_string(),
        request_id: "canonical-request".into(),
        correlation_epoch: "fixture-epoch".into(),
    };
    let tx = first.begin().await?;
    owner.bind_trajectory(&tx, &executions[3], &binding).await?;
    tx.commit().await?;
    let complete = owner
        .prepare_attempt(&key_hash, &executions[3], &stamp)
        .await?;
    owner
        .dispatch(&key_hash, &executions[3], &complete.attempt_id)
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
    owner
        .record_terminal(&executions[3], &complete.attempt_id, &terminal)
        .await?;
    let early_fence_key = hex::encode(sha2::Sha256::digest(serde_json::to_vec(&(
        "gateway-episode-fence-v1",
        &tenant,
        &binding.episode_id,
    ))?));
    let early_fence: Stored<serde_json::Value> = first
        .get(Kind::EpisodeFence, &early_fence_key)
        .await?
        .context("projection fence missing before finish")?;
    ensure!(
        early_fence.value["pending"] == serde_json::json!([executions[3]]),
        "provider permit was released without its cost barrier"
    );
    ensure!(matches!(
        reconciler.query(&key_hash, &executions[3]).await?.state,
        ExecutionState::Running
    ));
    let prepared = owner
        .prepare_attempt(&key_hash, &executions[1], &stamp)
        .await?;
    let dispatched = owner
        .prepare_attempt(&key_hash, &executions[2], &stamp)
        .await?;
    owner
        .dispatch(&key_hash, &executions[2], &dispatched.attempt_id)
        .await?;
    ensure!(
        owner
            .finish(
                &executions[2],
                &TerminalOutcome::Failed {
                    code: "transport_error".into()
                }
            )
            .await
            .is_err()
    );
    tokio::time::sleep(Duration::from_millis(1_050)).await;
    ensure!(
        owner.heartbeat().await.is_err(),
        "expired incarnation was resurrected"
    );
    let (one, two) = tokio::join!(reconciler.reconcile(32), reconciler.reconcile(32));
    one?;
    two?;
    let before = reconciler.query(&key_hash, &executions[0]).await?;
    ensure!(matches!(before.state, ExecutionState::Terminal { .. }));
    let abandoned = reconciler.query(&key_hash, &executions[1]).await?;
    ensure!(matches!(abandoned.state, ExecutionState::Terminal { .. }));
    ensure!(
        abandoned.attempts[0].attempt_id == prepared.attempt_id
            && matches!(abandoned.attempts[0].state, AttemptState::NotDispatched)
    );
    let unknown = reconciler.query(&key_hash, &executions[2]).await?;
    ensure!(
        matches!(unknown.state, ExecutionState::Unknown { .. })
            && matches!(unknown.attempts[0].state, AttemptState::Unknown { .. })
    );
    let recovered = reconciler.query(&key_hash, &executions[3]).await?;
    ensure!(
        matches!(recovered.state, ExecutionState::Terminal {ref result} if result.usage.cost_micro_usd == Some(7))
    );
    for kind in [Kind::Tenant, Kind::Resource] {
        let key = if kind == Kind::Tenant {
            tenant.as_str()
        } else {
            "recovery-pool"
        };
        let row: Stored<serde_json::Value> = first
            .get(kind, key)
            .await?
            .context("occupancy row missing")?;
        ensure!(
            row.value["active"] == 1,
            "unknown provider work did not retain exactly one permit"
        );
    }
    let event: Stored<serde_json::Value> = first
        .get(Kind::Outbox, &format!("gateway-terminal-{}", executions[3]))
        .await?
        .context("terminal outbox event missing")?;
    ensure!(event.value["event"]["execution"]["execution_id"] == executions[3]);
    let fence_key = hex::encode(sha2::Sha256::digest(serde_json::to_vec(&(
        "gateway-episode-fence-v1",
        &tenant,
        &binding.episode_id,
    ))?));
    let fence: Stored<serde_json::Value> = first
        .get(Kind::EpisodeFence, &fence_key)
        .await?
        .context("terminal projection fence missing")?;
    ensure!(fence.value["pending"] == serde_json::json!([executions[3]]));
    // An original worker may deliver late evidence, without acquiring a new
    // owner lease or dispatching again. Repeated finalization stays idempotent.
    owner
        .record_terminal(&executions[2], &dispatched.attempt_id, &terminal)
        .await?;
    let (first_finish, second_finish) = tokio::join!(
        owner.finish(&executions[2], &TerminalOutcome::Succeeded),
        owner.finish(&executions[2], &TerminalOutcome::Succeeded)
    );
    ensure!(first_finish? == second_finish?);
    let capacity: Stored<serde_json::Value> = first
        .get(Kind::Resource, "recovery-pool")
        .await?
        .context("resource missing")?;
    ensure!(capacity.value["active"] == 0);
    let occupancy: Stored<serde_json::Value> = first
        .get(Kind::Tenant, &tenant)
        .await?
        .context("tenant missing")?;
    ensure!(occupancy.value["active"] == 0);
    // Explicit reconciliation records an operator assertion, not fabricated
    // provider usage. A stale revision or late provider result cannot overwrite it.
    let operator_execution = match reconciler
        .admit(&Admission {
            requirements: crate::gateway::shared::capture::Requirements::fixture(),
            key_hash: &key_hash,
            correlation_id: "operator-case",
            idempotency_key: None,
            fingerprint: &fingerprint,
            operation: ModelOperation::Generation,
            revision,
        })
        .await?
    {
        AdmissionResult::Accepted(execution, _) => execution.execution_id,
        AdmissionResult::Existing(_) => anyhow::bail!("operator fixture unexpectedly deduplicated"),
    };
    let uncertain = reconciler
        .prepare_attempt(&key_hash, &operator_execution, &stamp)
        .await?;
    reconciler
        .dispatch(&key_hash, &operator_execution, &uncertain.attempt_id)
        .await?;
    reconciler
        .mark_unknown(&operator_execution, &uncertain.attempt_id)
        .await?;
    let unknown = reconciler.query(&key_hash, &operator_execution).await?;
    let mut request = bitrouter_ai::gateway::execution::ResolveAttemptRequest {
        expected_revision: 0,
        outcome: bitrouter_ai::gateway::execution::ResolutionOutcome::Terminated,
        evidence_reference: "provider-case-123".into(),
    };
    ensure!(
        reconciler
            .resolve_unknown(
                "operator-a",
                &operator_execution,
                &uncertain.attempt_id,
                &request
            )
            .await
            .is_err()
    );
    request.expected_revision = unknown.revision;
    let resolved = reconciler
        .resolve_unknown(
            "operator-a",
            &operator_execution,
            &uncertain.attempt_id,
            &request,
        )
        .await?;
    ensure!(matches!(resolved.state, ExecutionState::Resolved { .. }));
    ensure!(
        resolved
            == reconciler
                .resolve_unknown(
                    "operator-a",
                    &operator_execution,
                    &uncertain.attempt_id,
                    &request
                )
                .await?
    );
    ensure!(
        reconciler
            .record_terminal(&operator_execution, &uncertain.attempt_id, &terminal)
            .await
            .is_err()
    );
    let delivery_peer = Engine::join(
        Authority::new(second.clone(), DeploymentKey::fixture()?),
        30_000,
    )
    .await?;
    shared_outbox(&reconciler, &delivery_peer).await?;
    let occupancy: Stored<serde_json::Value> = first
        .get(Kind::Tenant, &tenant)
        .await?
        .context("tenant missing after delivery retries")?;
    ensure!(occupancy.value["active"] == 0);
    Ok(())
}

async fn shared_outbox(a: &Engine, b: &Engine) -> Result<()> {
    use std::collections::BTreeSet;
    let (left, right) = tokio::join!(a.claim_outbox(32, 200), b.claim_outbox(32, 200));
    let first = left?.into_iter().chain(right?).collect::<Vec<_>>();
    ensure!(
        first.len() == 5,
        "expected one immutable terminal event per finalized execution"
    );
    let ids = first
        .iter()
        .map(|claim| claim.event.event_id.clone())
        .collect::<BTreeSet<_>>();
    ensure!(
        ids.len() == first.len(),
        "competing delivery workers claimed the same lease"
    );
    tokio::time::sleep(Duration::from_millis(220)).await;
    let mut repeated = a.claim_outbox(32, 2_000).await?;
    ensure!(
        repeated
            .iter()
            .map(|claim| claim.event.event_id.clone())
            .collect::<BTreeSet<_>>()
            == ids,
        "lost ACK changed terminal event identity"
    );
    for claim in &first {
        ensure!(
            b.acknowledge_outbox(claim).await.is_err(),
            "stale delivery lease acknowledged a newer claim"
        );
    }
    let deferred = repeated.pop().context("no terminal delivery to defer")?;
    for claim in &repeated {
        a.acknowledge_outbox(claim).await?;
        b.acknowledge_outbox(claim).await?;
    }
    a.retry_outbox(&deferred, 150).await?;
    ensure!(
        b.claim_outbox(32, 2_000).await?.is_empty(),
        "retry delay was bypassed"
    );
    tokio::time::sleep(Duration::from_millis(170)).await;
    let delayed = b.claim_outbox(32, 2_000).await?;
    ensure!(delayed.len() == 1 && delayed[0].event == deferred.event);
    b.acknowledge_outbox(&delayed[0]).await?;
    ensure!(
        a.claim_outbox(32, 2_000).await?.is_empty(),
        "acknowledged terminal event was redelivered"
    );
    Ok(())
}

async fn namespace_and_rollback(first: &Store, second: &Store) -> Result<()> {
    let other = Store::new(
        first.database().clone(),
        &format!("other-{}", uuid::Uuid::new_v4()),
        Duration::from_secs(3),
    )?;
    let tx = first.begin().await?;
    let now = Store::now(&tx).await?;
    ensure!(
        first
            .create(
                &tx,
                NewRecord {
                    kind: Kind::Tenant,
                    key: "counter",
                    status: "ready",
                    due_at_ms: Some(now),
                    now_ms: now,
                    value: &Counter { value: 1 },
                }
            )
            .await?
    );
    tx.commit().await?;
    ensure!(
        other
            .get::<Counter>(Kind::Tenant, "counter")
            .await?
            .is_none()
    );
    let tx = first.begin().await?;
    let mut counter: Stored<Counter> = first.lock(&tx, Kind::Tenant, "counter").await?;
    ensure!(counter.revision()? == 0);
    counter.value.value = 99;
    first
        .save(&tx, &mut counter, "ready", Some(now), now)
        .await?;
    tx.rollback().await?;
    let counter: Stored<Counter> = second
        .get(Kind::Tenant, "counter")
        .await?
        .context("committed counter missing")?;
    ensure!(counter.value.value == 1 && counter.revision()? == 0);
    let due = second.due::<Counter>(Kind::Tenant, "ready", now, 1).await?;
    ensure!(due.len() == 1 && due[0].key() == "counter");
    ensure!(
        second
            .due::<Counter>(Kind::Tenant, "ready", now, 257)
            .await
            .is_err()
    );
    // An aborted transaction must not publish either part of a multi-row write.
    let tx = first.begin().await?;
    ensure!(
        first
            .create(
                &tx,
                NewRecord {
                    kind: Kind::Execution,
                    key: "rolled-back",
                    status: "accepted",
                    due_at_ms: Some(now),
                    now_ms: now,
                    value: &Counter { value: 2 },
                }
            )
            .await?
    );
    tx.rollback().await?;
    ensure!(
        second
            .get::<Counter>(Kind::Execution, "rolled-back")
            .await?
            .is_none()
    );
    Ok(())
}

async fn key_authority_and_publication_fence(first: &Store, second: &Store) -> Result<()> {
    let a = Authority::new(first.clone(), DeploymentKey::fixture()?);
    let b = Authority::new(second.clone(), DeploymentKey::fixture()?);
    ensure!(
        b.head().await.is_err(),
        "joining must not generate independent keys"
    );
    let initial = a.initialize().await?;
    ensure!(
        b.initialize().await.is_err(),
        "initialization must not overwrite an authority"
    );
    let key_a = a.keys(&initial).await?.active_correlation()?;
    let key_b = b.keys(&b.head().await?).await?.active_correlation()?;
    ensure!(key_a.key_id() == key_b.key_id());
    let first_continuation = format!("{:?}", a.keys(&initial).await?.continuation()?);
    ensure!(first_continuation == format!("{:?}", b.keys(&initial).await?.continuation()?));
    let wrong_key = Authority::new(second.clone(), DeploymentKey::from_bytes(rand::random())?);
    ensure!(wrong_key.head().await.is_err());

    let reader_a = first.begin().await?;
    a.fence(&reader_a).await?;
    // A second independent connection can hold the same shared fence at once.
    let reader_b = second.begin().await?;
    tokio::time::timeout(Duration::from_millis(300), b.fence(&reader_b)).await??;
    let publisher = b.clone();
    let mut publication = tokio::spawn(async move { publisher.rotate_correlation(1).await });
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), &mut publication)
            .await
            .is_err(),
        "publication crossed an active admission fence"
    );
    reader_a.commit().await?;
    ensure!(
        tokio::time::timeout(Duration::from_millis(100), &mut publication)
            .await
            .is_err(),
        "publication ignored another reader fence"
    );
    reader_b.commit().await?;
    let rotated = publication.await??;
    ensure!(rotated.key_generation == 2);
    let keys = b.keys(&rotated).await?;
    ensure!(keys.correlations()?.len() == 2);
    ensure!(keys.correlations()?[0].key_id() == key_a.key_id());
    ensure!(keys.active_correlation()?.key_id() != key_a.key_id());
    ensure!(format!("{:?}", keys.continuation()?) == first_continuation);
    ensure!(
        a.rotate_correlation(1).await.is_err(),
        "stale publication must fail"
    );
    ensure!(a.head().await?.key_generation == 2);
    // Only ciphertext is persisted for the service-private key ring.
    let row = first
        .database()
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "SELECT body FROM gateway_shared_records WHERE namespace = $1 AND key = 'keys/2'",
            [first.namespace().into()],
        ))
        .await?
        .context("encrypted key record missing")?;
    let body: String = row.try_get("", "body")?;
    ensure!(
        body.contains("ciphertext")
            && !body.contains("correlations")
            && !body.contains("continuation")
    );
    Ok(())
}

async fn transaction_timeout(url: &str, store: &Store) -> Result<()> {
    let single = store::connect(url, 1, Duration::from_secs(2)).await?;
    let bounded = Store::new(
        single.clone(),
        store.namespace(),
        Duration::from_millis(150),
    )?;
    let result = bounded
        .bounded(async {
            let tx = bounded.begin().await?;
            let mut counter: Stored<Counter> = bounded.lock(&tx, Kind::Tenant, "counter").await?;
            counter.value.value = 99;
            bounded
                .save(&tx, &mut counter, "ready", None, Store::now(&tx).await?)
                .await?;
            tx.execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT pg_sleep(1)".to_owned(),
            ))
            .await?;
            tx.commit().await?;
            Ok(())
        })
        .await;
    ensure!(result.is_err());
    // Cancellation/rollback returns the pool to service instead of retaining locks.
    let recovered = Store::new(single.clone(), store.namespace(), Duration::from_secs(2))?;
    let counter: Stored<Counter> = recovered
        .get(Kind::Tenant, "counter")
        .await?
        .context("counter missing after cancelled write")?;
    ensure!(
        counter.value.value == 1 && counter.revision()? == 0,
        "cancelled transaction leaked a write"
    );
    single.close().await?;
    Ok(())
}

async fn pooled_read_timeout(url: &str, blocker: &Store) -> Result<()> {
    let single = store::connect(url, 1, Duration::from_secs(2)).await?;
    let reader = Store::new(
        single.clone(),
        blocker.namespace(),
        Duration::from_millis(150),
    )?;
    let authority = Authority::new(reader.clone(), DeploymentKey::fixture()?);
    let blocked = blocker.begin().await?;
    blocked
        .execute(Statement::from_string(
            DatabaseBackend::Postgres,
            "LOCK TABLE gateway_shared_records IN ACCESS EXCLUSIVE MODE".to_owned(),
        ))
        .await?;
    let result = async {
        ensure!(authority.head().await.is_err());
        // Keep the table lock held. Only server cancellation plus rollback can
        // return the sole pool connection; a spare connection cannot hide a leak.
        tokio::time::timeout(Duration::from_secs(2), async {
            let tx = reader.begin().await?;
            Store::now(&tx).await?;
            tx.commit().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("timed-out pooled read stranded the only connection")??;
        ensure!(
            reader
                .due::<Counter>(Kind::Tenant, "ready", i64::MAX, 1)
                .await
                .is_err()
        );
        tokio::time::timeout(Duration::from_secs(2), async {
            let tx = reader.begin().await?;
            Store::now(&tx).await?;
            tx.commit().await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("timed-out due scan stranded the only connection")??;
        Ok::<(), anyhow::Error>(())
    }
    .await;
    blocked.rollback().await?;
    single.close().await?;
    result
}

async fn connector_trace_is_redacted(url: &str) -> Result<()> {
    use std::sync::{Arc, Mutex};
    use tracing::instrument::WithSubscriber;
    const SECRET: &str = "fixture-password-never-in-connector-trace";
    struct Writer(Arc<Mutex<Vec<u8>>>);
    impl std::io::Write for Writer {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0
                .lock()
                .map_err(|_| std::io::Error::other("capture lock poisoned"))?
                .extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let bytes = Arc::new(Mutex::new(Vec::new()));
    let writer = bytes.clone();
    let subscriber = tracing_subscriber::fmt()
        .without_time()
        .with_ansi(false)
        .with_max_level(tracing::Level::TRACE)
        .with_span_events(
            tracing_subscriber::fmt::format::FmtSpan::NEW
                | tracing_subscriber::fmt::format::FmtSpan::CLOSE,
        )
        .with_writer(move || Writer(writer.clone()))
        .finish();
    let mut address = url::Url::parse(url)?;
    address
        .set_password(Some(SECRET))
        .map_err(|_| anyhow::anyhow!("fixture URL cannot carry password"))?;
    async {
        tracing::trace!("capture_probe");
        ensure!(
            store::connect(address.as_str(), 1, Duration::from_secs(2))
                .await
                .is_err()
        );
        Ok::<(), anyhow::Error>(())
    }
    .with_subscriber(subscriber)
    .await?;
    let captured = String::from_utf8(
        bytes
            .lock()
            .map_err(|_| anyhow::anyhow!("capture lock poisoned"))?
            .clone(),
    )?;
    ensure!(
        captured.contains("capture_probe"),
        "trace-capture regression was disabled"
    );
    ensure!(
        !captured.contains(SECRET),
        "database credential appeared in connector trace"
    );
    Ok(())
}
