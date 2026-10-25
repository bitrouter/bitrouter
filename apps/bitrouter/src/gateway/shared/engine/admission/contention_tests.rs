//! A committed heartbeat between admission's snapshot and owner check must not
//! force every admission attempt to abort. No sleeps determine the interleaving.

use crate::runtime_crypto::DeploymentKey;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Duration;

use super::*;
use crate::auth::db::{NewApiKey, insert_api_key, upsert_user};
use crate::gateway::shared::{
    authority::Authority,
    engine::{Heartbeat, TenantLimits},
    schema,
    test_database::EmptyDatabase,
};

struct Interleaving {
    heartbeat: Heartbeat,
    attempts: AtomicUsize,
}

tokio::task_local! {
    static CURRENT: Arc<Interleaving>;
}

pub(super) async fn before_owner() -> Result<()> {
    if let Ok(interleaving) = CURRENT.try_with(Arc::clone) {
        interleaving.attempts.fetch_add(1, Ordering::SeqCst);
        // This uses a separate pool and commits before the admission continues.
        interleaving.heartbeat.renew().await?;
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_admission_survives_heartbeat_single_owner() -> Result<()> {
    exercise(1).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_admission_survives_heartbeat_two_owners() -> Result<()> {
    exercise(2).await
}

async fn exercise(owners: usize) -> Result<()> {
    const CALLS: u32 = 16;
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let timeout = Duration::from_secs(3);
        let store = Store::new(
            database.connect(16, timeout).await?,
            "admission-contention",
            timeout,
        )?;
        schema::initialize_all(&store).await?;
        let authority = Authority::new(store.clone(), DeploymentKey::fixture()?);
        authority.initialize().await?;
        upsert_user(store.database(), "tenant").await?;
        let key_hash = crate::auth::keys::hash_key("fixture-concurrent-admission");
        insert_api_key(
            store.database(),
            &NewApiKey {
                id: "principal".into(),
                key_hash: key_hash.clone(),
                user_id: "tenant".into(),
                spend_limit_micro_usd: None,
                rpm_limit: Some(CALLS.into()),
                policy_id: None,
            },
        )
        .await?;
        let mut engines = Vec::new();
        let mut interleavings = Vec::new();
        for _ in 0..owners {
            let pool = database.connect(16, timeout).await?;
            let engine = Engine::join(authority.with_database(pool)?, 30_000).await?;
            interleavings.push(Arc::new(Interleaving {
                heartbeat: engine.heartbeat_handle(database.connect(2, timeout).await?)?,
                attempts: AtomicUsize::new(0),
            }));
            engines.push(engine);
        }
        engines[0]
            .set_tenant(
                "tenant",
                None,
                &TenantLimits {
                    concurrent_executions: CALLS,
                    requests_per_minute: CALLS,
                    resource_pools: ["pool".into()].into(),
                },
            )
            .await?;
        let revision = authority.head().await?.revision();
        let results = futures::future::join_all((0..CALLS).map(|index| {
            let engine = &engines[index as usize % owners];
            let key_hash = &key_hash;
            let interleaving = interleavings[index as usize % owners].clone();
            CURRENT.scope(interleaving, async move {
                let identity = format!("request-{index}");
                engine
                    .admit_with_id(
                        &identity,
                        &Admission {
                            requirements: crate::gateway::shared::capture::Requirements::fixture(),
                            key_hash,
                            correlation_id: &identity,
                            idempotency_key: Some(&identity),
                            fingerprint: &"a".repeat(64),
                            operation: bitrouter_ai::types::ModelOperation::Generation,
                            revision,
                        },
                    )
                    .await
            })
        }))
        .await;
        let attempts: usize = interleavings
            .iter()
            .map(|entry| entry.attempts.load(Ordering::SeqCst))
            .sum();
        eprintln!("{owners} owner(s): {attempts} owner-check interleavings for {CALLS} admissions");
        let mut identities = std::collections::BTreeSet::new();
        let mut accepted_by_minute = std::collections::BTreeMap::<i64, u32>::new();
        for result in results {
            let AdmissionResult::Accepted(view, _) = result? else {
                anyhow::bail!("distinct admission was deduplicated");
            };
            *accepted_by_minute
                .entry(view.accepted_at_ms.div_euclid(60_000))
                .or_default() += 1;
            ensure!(identities.insert(view.execution_id));
        }
        ensure!(
            attempts == CALLS as usize,
            "heartbeat forced transaction retries"
        );
        let tenant: Stored<Tenant> = store
            .get(Kind::Tenant, "tenant")
            .await?
            .context("missing tenant")?;
        ensure!(tenant.value.active == CALLS);
        ensure!(Some(&tenant.value.accepted) == accepted_by_minute.get(&tenant.value.minute));
        let bucket: Stored<RateBucket> = store
            .get(Kind::Principal, "principal")
            .await?
            .context("missing rate bucket")?;
        ensure!(Some(&bucket.value.accepted) == accepted_by_minute.get(&bucket.value.minute));
        for identity in identities {
            let binding: Stored<Idempotency> = store
                .get(Kind::Idempotency, &idempotency_id("tenant", &identity)?)
                .await?
                .context("missing idempotency")?;
            ensure!(binding.value.execution_id == identity);
            ensure!(engines[0].query(&key_hash, &identity).await?.execution_id == identity);
        }
        Ok(())
    })
    .await
    .context("admission contention campaign deadline")
    .and_then(|result| result);
    database.finish(result).await
}
