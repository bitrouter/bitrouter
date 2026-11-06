//! Dispatches share a policy fence, but cannot race a resource policy update.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::AttemptState;
use bitrouter_ai::gateway::operator::ResourcePut;
use bitrouter_ai::types::ModelOperation;
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr, RuntimeErr, SqlxError, Statement};
use tokio::sync::{Semaphore, mpsc};

use super::super::{Admission, AdmissionResult, Engine, Rejection, Resource, TenantLimits};
use crate::auth::db::{NewApiKey, insert_api_key, upsert_user};
use crate::gateway::providers::auth::{credentials::Credential, store::CredentialKey};
use crate::gateway::shared::{
    authority::Authority,
    capture::Requirements,
    credentials::Credentials,
    schema,
    store::{Kind, Store, Stored},
    test_database::EmptyDatabase,
};
use crate::runtime_crypto::DeploymentKey;

struct Interleaving {
    arrived: mpsc::Sender<()>,
    release: Arc<Semaphore>,
}

tokio::task_local! {
    static CURRENT: Interleaving;
}

pub(super) async fn after_resource() -> Result<()> {
    if let Ok((arrived, release)) =
        CURRENT.try_with(|state| (state.arrived.clone(), state.release.clone()))
    {
        arrived.send(()).await?;
        release.acquire().await?.forget();
    }
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_dispatch_shares_resource_fence_single_owner() -> Result<()> {
    exercise(1).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_dispatch_shares_resource_fence_two_owners() -> Result<()> {
    exercise(2).await
}

async fn exercise(owners: usize) -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let timeout = Duration::from_secs(3);
        let store = Store::new(
            database.connect(8, timeout).await?,
            "dispatch-contention",
            timeout,
        )?;
        schema::initialize_all(&store).await?;
        let authority = Authority::new(store.clone(), DeploymentKey::from_bytes(rand::random())?);
        authority.initialize().await?;
        upsert_user(store.database(), "tenant").await?;
        let key_hash = crate::auth::keys::hash_key(&uuid::Uuid::new_v4().to_string());
        insert_api_key(
            store.database(),
            &NewApiKey {
                id: "principal".into(),
                key_hash: key_hash.clone(),
                user_id: "tenant".into(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        let mut engines = Vec::new();
        for _ in 0..owners {
            engines.push(
                Engine::join(
                    authority.with_database(database.connect(8, timeout).await?)?,
                    30_000,
                )
                .await?,
            );
        }
        engines[0]
            .set_tenant(
                "tenant",
                None,
                &TenantLimits {
                    concurrent_executions: 3,
                    requests_per_minute: 100,
                    resource_pools: ["pool".into()].into(),
                },
            )
            .await?;
        engines[0].set_resource("pool", None, 3).await?;
        let stamp = Credentials::new(authority.clone())
            .import(
                &CredentialKey {
                    provider: "fixture".into(),
                    account: "default".into(),
                },
                "pool",
                None,
                &Credential::api_key(uuid::Uuid::new_v4().to_string()),
            )
            .await?;
        let revision = authority.head().await?.revision();
        let mut work = Vec::new();
        for index in 0..3 {
            let engine = &engines[index % owners];
            let identity = format!("request-{index}");
            let AdmissionResult::Accepted(execution, _) = engine
                .admit_with_id(
                    &identity,
                    &Admission {
                        requirements: Requirements::fixture(),
                        key_hash: &key_hash,
                        correlation_id: &identity,
                        idempotency_key: None,
                        fingerprint: &"a".repeat(64),
                        operation: ModelOperation::Generation,
                        revision,
                    },
                )
                .await?
            else {
                anyhow::bail!("distinct admission was deduplicated");
            };
            let attempt = engine
                .prepare_attempt(&key_hash, &execution.execution_id, &stamp)
                .await?;
            work.push((engine, execution.execution_id, attempt.attempt_id));
        }
        let before: Stored<Resource> = store.get(Kind::Resource, "pool").await?.context("pool")?;
        ensure!(before.value.active == 3);
        let (arrived, mut arrivals) = mpsc::channel(2);
        let release = Arc::new(Semaphore::new(0));
        let readers = futures::future::join_all(work[..2].iter().map(|(engine, id, attempt)| {
            CURRENT.scope(
                Interleaving {
                    arrived: arrived.clone(),
                    release: release.clone(),
                },
                engine.dispatch(&key_hash, id, attempt),
            )
        }));
        let controller = async {
            let result = async {
                // Both actual dispatch transactions must hold the fence at once.
                tokio::time::timeout(Duration::from_secs(2), async {
                    arrivals.recv().await.context("first dispatch missing")?;
                    arrivals.recv().await.context("second dispatch missing")?;
                    Ok::<(), anyhow::Error>(())
                })
                .await
                .context("resource readers serialized distinct dispatches")??;
                policy_writer_is_fenced(&authority).await
            }
            .await;
            // Also release a first reader when the second one cannot arrive.
            release.add_permits(2);
            result
        };
        let (results, controlled) = tokio::join!(readers, controller);
        controlled?;
        for result in results {
            result?;
        }
        let after: Stored<Resource> = store.get(Kind::Resource, "pool").await?.context("pool")?;
        ensure!(after.revision()? == before.revision()? && after.value.active == 3);
        for (engine, id, attempt) in &work[..2] {
            ensure!(matches!(
                engine.query(&key_hash, id).await?.attempts[0].state,
                AttemptState::Dispatched
            ));
            ensure!(
                engine.dispatch(&key_hash, id, attempt).await.is_err(),
                "repeated dispatch authorized another send"
            );
        }
        // The same writer can commit after dispatch releases its shared locks.
        ensure!(engines[0].set_resource("pool", Some(1), 3).await? == 2);
        let (engine, id, attempt) = &work[2];
        let error = engine
            .dispatch(&key_hash, id, attempt)
            .await
            .err()
            .context("stale resource policy crossed dispatch fence")?;
        ensure!(matches!(
            error.downcast_ref::<Rejection>(),
            Some(Rejection::AuthorityChanged)
        ));
        ensure!(matches!(
            engine.query(&key_hash, id).await?.attempts[0].state,
            AttemptState::Prepared
        ));
        Ok(())
    })
    .await
    .context("dispatch contention campaign deadline")
    .and_then(|result| result);
    database.finish(result).await
}

async fn policy_writer_is_fenced(authority: &Authority) -> Result<()> {
    let tx = authority.store().begin().await?;
    // The readers remain held until this real policy writer returns. A short
    // lock deadline makes refusal deterministic without relying on a sleep.
    tx.execute(Statement::from_string(
        DatabaseBackend::Postgres,
        "SET LOCAL lock_timeout = '1ms'".to_owned(),
    ))
    .await?;
    let result = super::super::management::put_resource(
        authority,
        &tx,
        &ResourcePut {
            resource_pool_id: "pool".into(),
            expected_revision: Some(1),
            concurrent_attempts: 3,
        },
    )
    .await;
    tx.rollback().await?;
    let error = result
        .err()
        .context("policy writer crossed dispatch fence")?;
    ensure!(error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<DbErr>(),
            Some(DbErr::Exec(RuntimeErr::SqlxError(SqlxError::Database(database)))
                | DbErr::Query(RuntimeErr::SqlxError(SqlxError::Database(database))))
                if database.code().as_deref() == Some("55P03")
        )
    }));
    Ok(())
}
