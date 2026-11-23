//! Supported publication and ordinary execution retain transactional guarantees.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::{
    AttemptState, ExecutionState, ExecutionUsage, GatewayExecution, TerminalOutcome,
};
use bitrouter_ai::types::{ModelOperation, UsageOrigin};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use serde_json::json;

use super::{
    Admission, AdmissionResult, Engine, Execution, Outbox, Resource, Tenant, TenantLimits,
};
use crate::auth::db::{NewApiKey, insert_api_key, upsert_user};
use crate::gateway::providers::auth::{credentials::Credential, store::CredentialKey};
use crate::gateway::shared::{
    authority::Authority,
    capture::Requirements,
    credentials::{CredentialStamp, Credentials},
    publication::Configuration,
    schema,
    store::{Kind, Store, Stored},
    test_database::{EmptyDatabase, combine_results},
};
use crate::metering::shared::{SharedMetering, evidence::Attribution};
use crate::runtime_crypto::DeploymentKey;

struct Fixture {
    engine: Engine,
    observer: Store,
    key_hash: String,
    credential: CredentialStamp,
    publication: Configuration,
}

impl Fixture {
    async fn new(database: &mut EmptyDatabase) -> Result<Self> {
        let timeout = Duration::from_secs(5);
        let namespace = format!("contract-{}", uuid::Uuid::new_v4());
        let store = Store::new(database.connect(4, timeout).await?, &namespace, timeout)?;
        let observer = Store::new(database.connect(2, timeout).await?, &namespace, timeout)?;
        schema::initialize_all(&store).await?;
        let authority = Authority::new(store, DeploymentKey::from_bytes(rand::random())?);
        authority.initialize().await?;
        let engine = Engine::join(authority.clone(), 180_000).await?;
        upsert_user(authority.store().database(), "tenant").await?;
        let key_hash = crate::auth::keys::hash_key(&uuid::Uuid::new_v4().to_string());
        insert_api_key(
            authority.store().database(),
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
        engine
            .set_tenant(
                "tenant",
                None,
                &TenantLimits {
                    concurrent_executions: 16,
                    requests_per_minute: 100,
                    resource_pools: ["pool".into()].into(),
                },
            )
            .await?;
        engine.set_resource("pool", None, 4).await?;
        let credential = Credentials::new(authority)
            .import(
                &CredentialKey {
                    provider: "fixture".into(),
                    account: "default".into(),
                },
                "pool",
                None,
                &Credential::api_key("owned-contract-fixture"),
            )
            .await?;
        let publication = Configuration {
            format: 2,
            routing: json!({"trajectory": {"enabled": true}}),
            policies: vec![],
            model_policy: Some(crate::model_policy::parse(&json!({
                "spec_version": "policy/1.0.0-beta.1",
                "policies": {"auto": {
                    "tiers": {"standard": {"model": "fixture:small"}},
                    "default_tier": "standard"
                }}
            }))?),
            credentials: vec![],
            require_known_pricing: false,
        };
        engine.authority.publish(0, &publication).await?;
        Ok(Self {
            engine,
            observer,
            key_hash,
            credential,
            publication,
        })
    }

    async fn admit(&self) -> Result<String> {
        let config = self.publication.prepare()?.config;
        match self
            .engine
            .admit(&Admission {
                requirements: Requirements::from_config(
                    &config,
                    self.publication.model_policy.as_ref(),
                )?,
                key_hash: &self.key_hash,
                correlation_id: "contract-fixture",
                idempotency_key: None,
                fingerprint: &"a".repeat(64),
                operation: ModelOperation::Generation,
                revision: self.engine.authority.head().await?.revision(),
            })
            .await?
        {
            AdmissionResult::Accepted(view, _) => Ok(view.execution_id),
            AdmissionResult::Existing(_) => anyhow::bail!("fresh admission was deduplicated"),
        }
    }

    async fn snapshot(&self) -> Result<Vec<String>> {
        let tx = self.observer.begin().await?;
        let mut rows = Vec::new();
        for table in [
            "gateway_shared_records",
            "gateway_shared_trajectory_episodes",
            "gateway_shared_trajectory_requests",
            "gateway_shared_trajectory_events",
            "gateway_shared_trajectory_prefix_index",
            "gateway_shared_trajectory_outbox",
            "gateway_shared_metering_requests",
            "gateway_shared_eval_subjects",
            "gateway_shared_eval_results",
            "gateway_shared_eval_admission_events",
        ] {
            // Table names are fixed; the namespace is always bound. Include
            // revisions and metadata so a rejected operation cannot hide writes.
            let sql = format!(
                "SELECT COALESCE(jsonb_agg(to_jsonb(item) ORDER BY to_jsonb(item)::text), '[]'::jsonb)::text AS contents FROM {table} AS item WHERE namespace=$1"
            );
            let row = tx
                .query_one(Statement::from_sql_and_values(
                    DatabaseBackend::Postgres,
                    sql,
                    [self.observer.namespace().into()],
                ))
                .await?
                .context("snapshot missing")?;
            rows.push(row.try_get("", "contents")?);
        }
        tx.commit().await?;
        Ok(rows)
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_invalid_source_publication_rejection_preserves_authority() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(&mut database).await?;
        let head = f.engine.authority.head().await?;
        let before = f.snapshot().await?;
        let mut rejected = f.publication.clone();
        let mut invalid = rejected.model_policy.take().context("source missing")?;
        invalid
            .policies
            .get_mut("auto")
            .context("policy missing")?
            .default_tier = "missing".into();
        rejected.model_policy = Some(invalid);
        let error = f
            .engine
            .authority
            .publish(head.config_generation, &rejected)
            .await
            .err()
            .context("legacy publication accepted")?;
        ensure!(error.to_string().contains("default tier"));
        ensure!(f.snapshot().await? == before);
        let retained = f.engine.authority.head().await?;
        ensure!(retained.revision() == head.revision());
        let published = f
            .engine
            .authority
            .publish(head.config_generation, &f.publication)
            .await?;
        ensure!(published.config_generation == head.config_generation + 1);
        f.engine
            .authority
            .configuration(&published)
            .await?
            .prepare()?;
        Ok(())
    })
    .await
    .context("publication contract deadline")
    .and_then(|result| result);
    database.finish(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_join_checks_old_retained_publication_and_terminal_beyond_first_page() -> Result<()>
{
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(90), async {
        let f = Fixture::new(&mut database).await?;
        for generation in 1..33 {
            f.engine
                .authority
                .publish(generation, &f.publication)
                .await?;
        }
        let mut executions = Vec::new();
        for _ in 0..33 {
            let id = f.admit().await?;
            let outcome = TerminalOutcome::Failed {
                code: "before_provider_send".into(),
            };
            no_attempt_failure(
                &f.engine.finish_with_inputs(&id, &outcome, None).await?,
                &outcome,
            )?;
            executions.push(id);
        }
        executions.sort();
        let last = executions.last().context("last retained execution")?;
        // Both readers use 32-row keyset pages. Configuration/9 sorts last;
        // the terminal attack is also past a full healthy first page.
        let healthy = Engine::join(f.engine.authority.clone(), 180_000).await?;
        healthy.drain().await?;
        let key = "configuration/9";
        let original: Stored<crate::runtime_crypto::SealedSecret> = f
            .observer
            .get(Kind::Authority, key)
            .await?
            .context("retained publication")?;
        let mut old = serde_json::to_value(&f.publication)?;
        old["format"] = json!(1);
        old["policy_lock"] = old["model_policy"].take();
        old.as_object_mut()
            .context("publication object")?
            .remove("model_policy");
        let sealed = f
            .engine
            .authority
            .seal(key, 9, &serde_json::to_vec(&old)?)?;
        replace_contract_body(&f, "authority", key, &serde_json::to_value(sealed)?).await?;
        refused_join_preserves_namespace(&f, "retained shared publication").await?;
        replace_contract_body(&f, "authority", key, &serde_json::to_value(original.value)?).await?;

        let terminal: Stored<serde_json::Value> = f
            .observer
            .get(Kind::Execution, last)
            .await?
            .context("terminal history")?;
        let mut old = terminal.value;
        old["projection"] = serde_json::Value::Null;
        replace_contract_body(&f, "execution", last, &old).await?;
        refused_join_preserves_namespace(&f, "original projection requirements are missing")
            .await?;
        Ok(())
    })
    .await
    .context("retained history join deadline")
    .and_then(|result| result);
    database.finish(result).await
}

async fn replace_contract_body(
    f: &Fixture,
    kind: &str,
    key: &str,
    body: &serde_json::Value,
) -> Result<()> {
    let result = f
        .observer
        .database()
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "UPDATE gateway_shared_records SET body=$1 WHERE namespace=$2 AND kind=$3 AND key=$4",
            [
                serde_json::to_string(body)?.into(),
                f.observer.namespace().into(),
                kind.into(),
                key.into(),
            ],
        ))
        .await?;
    ensure!(result.rows_affected() == 1);
    Ok(())
}

async fn refused_join_preserves_namespace(f: &Fixture, reason: &str) -> Result<()> {
    let before = f.snapshot().await?;
    let error = Engine::join(f.engine.authority.clone(), 180_000)
        .await
        .err()
        .context("old retained history joined")?;
    ensure!(
        format!("{error:#}").contains(reason),
        "unexpected refusal: {error:#}"
    );
    ensure!(
        f.snapshot().await? == before,
        "refused join changed authority or history"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_capture_identity_rejects_before_attempt_or_terminal_writes() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(&mut database).await?;
        let id = f.admit().await?;
        let ready = f.engine.fixture_projection_inputs(&id).await?;
        let failure = TerminalOutcome::Failed {
            code: "fixture_before_dispatch".into(),
        };
        let attribution = Attribution::fixture();
        for (field, value) in [
            ("execution_id", json!("other-execution")),
            ("owner", json!("other-owner")),
            (
                "operation",
                serde_json::to_value(ModelOperation::Decisions)?,
            ),
        ] {
            let mut changed = serde_json::to_value(&ready)?;
            changed[field] = value;
            let changed = serde_json::from_value(changed)?;
            let before = f.snapshot().await?;
            let error = f
                .engine
                .prepare_observed_attempt(&f.key_hash, &id, &f.credential, &attribution, &changed)
                .await
                .err()
                .context("mismatched capture prepared an attempt")?;
            ensure!(
                error
                    .to_string()
                    .contains("projection input identity mismatch")
            );
            ensure!(
                f.snapshot().await? == before,
                "{field} mismatch wrote attempt state"
            );
            let error = f
                .engine
                .finish_with_inputs(&id, &failure, Some(&changed))
                .await
                .err()
                .context("mismatched capture finalized execution")?;
            ensure!(
                error
                    .to_string()
                    .contains("projection input identity mismatch")
            );
            ensure!(
                f.snapshot().await? == before,
                "{field} mismatch wrote terminal state"
            );
        }
        // A valid capture still reserves capacity. Finalization before dispatch
        // releases that reservation and never invents a provider observation.
        f.engine
            .prepare_observed_attempt(&f.key_hash, &id, &f.credential, &attribution, &ready)
            .await?;
        let finished = f
            .engine
            .finish_with_inputs(&id, &failure, Some(&ready))
            .await?;
        ensure!(matches!(finished.state, ExecutionState::Terminal { .. }));
        ensure!(finished.attempts.len() == 1);
        ensure!(
            finished
                .attempts
                .first()
                .context("prepared attempt missing")?
                .state
                == AttemptState::NotDispatched
        );
        let resource: Stored<Resource> = f
            .observer
            .get(Kind::Resource, "pool")
            .await?
            .context("resource missing")?;
        let tenant: Stored<Tenant> = f
            .observer
            .get(Kind::Tenant, "tenant")
            .await?
            .context("tenant missing")?;
        ensure!(
            resource.value.active == 0 && tenant.value.active == 0,
            "finalization leaked capacity"
        );
        let empty_id = f.admit().await?;
        let finished = f
            .engine
            .finish_with_inputs(&empty_id, &failure, None)
            .await?;
        no_attempt_failure(&finished, &failure)?;
        let event: Stored<Outbox> = f
            .observer
            .get(Kind::Outbox, &format!("gateway-terminal-{empty_id}"))
            .await?
            .context("failure outbox missing")?;
        f.engine.deliver(&event.value.event).await?;
        let tx = f.observer.begin().await?;
        let record = SharedMetering::new(f.observer.namespace())?
            .get_in(&tx, "tenant", "principal", &empty_id)
            .await?
            .context("failure metering missing")?;
        tx.rollback().await?;
        ensure!(record.event.execution == finished && record.attempts.is_empty());
        let accounting = record.accounting()?;
        ensure!(accounting.known_attempts == 0 && accounting.unknown_attempts == 0);
        ensure!(accounting.known_subtotal_micro_usd == 0);
        let receipt: Stored<serde_json::Value> = f
            .observer
            .get(Kind::ProjectionReceipt, &event.value.event.event_id)
            .await?
            .context("failure receipt missing")?;
        ensure!(receipt.status() == "delivered");
        let delivered = f.snapshot().await?;
        f.engine.deliver(&event.value.event).await?;
        ensure!(
            f.snapshot().await? == delivered,
            "duplicate failure delivery wrote state"
        );
        Ok(())
    })
    .await
    .context("capture contract deadline")
    .and_then(|result| result);
    database.finish(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL CREATEDB privilege"]
async fn postgres_read_committed_terminal_invalidates_serializable_execution_snapshot() -> Result<()>
{
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(&mut database).await?;
        let id = f.admit().await?;
        let stale = f.observer.begin().await?;
        let result = async {
            // This awaited MVCC read fixes the old snapshot without holding a
            // row lock that could block the Read Committed terminal writer.
            let before: Stored<Execution> = f
                .observer
                .read(&stale, Kind::Execution, &id)
                .await?
                .context("execution missing")?;
            ensure!(matches!(before.value.view.state, ExecutionState::Accepted));
            let failure = TerminalOutcome::Failed {
                code: "fixture_before_dispatch".into(),
            };
            let finished = f.engine.finish_with_inputs(&id, &failure, None).await?;
            no_attempt_failure(&finished, &failure)?;
            let execution: Stored<Execution> = f
                .observer
                .get(Kind::Execution, &id)
                .await?
                .context("terminal execution missing")?;
            ensure!(execution.value.view == finished && execution.revision()? == finished.revision);
            ensure!(execution.value.trajectory.is_none());
            let outbox: Stored<Outbox> = f
                .observer
                .get(Kind::Outbox, &format!("gateway-terminal-{id}"))
                .await?
                .context("terminal outbox missing")?;
            ensure!(
                outbox.value.event.tenant_id == "tenant"
                    && outbox.value.event.execution == finished
            );
            let committed = f.snapshot().await?;
            let error = f
                .observer
                .lock::<Execution>(&stale, Kind::Execution, &id)
                .await
                .err()
                .context("stale execution snapshot remained lockable")?;
            ensure!(
                is_serialization_abort(&error),
                "expected SQLSTATE 40001, got {error:#}"
            );
            Ok(committed)
        }
        .await;
        let released = stale.rollback().await.map_err(anyhow::Error::from);
        let committed = match result {
            Ok(committed) => {
                released?;
                committed
            }
            Err(error) => return combine_results(Err(error), vec![released]),
        };
        ensure!(
            f.snapshot().await? == committed,
            "stale abort changed durable state"
        );
        Ok(())
    })
    .await
    .context("terminal snapshot deadline")
    .and_then(|result| result);
    database.finish(result).await
}

fn is_serialization_abort(error: &anyhow::Error) -> bool {
    use sea_orm::{DbErr, RuntimeErr, SqlxError};
    error.chain().any(|cause| {
        matches!(
            cause.downcast_ref::<DbErr>(),
            Some(DbErr::Exec(RuntimeErr::SqlxError(SqlxError::Database(database)))
                | DbErr::Query(RuntimeErr::SqlxError(SqlxError::Database(database))))
                if database.code().as_deref() == Some("40001")
        )
    })
}

fn no_attempt_failure(execution: &GatewayExecution, outcome: &TerminalOutcome) -> Result<()> {
    ensure!(execution.attempts.is_empty());
    let ExecutionState::Terminal { result } = &execution.state else {
        anyhow::bail!("failure was not terminal");
    };
    ensure!(result.outcome == *outcome);
    ensure!(
        result.usage
            == ExecutionUsage {
                origin: UsageOrigin::Unknown,
                prompt_tokens: None,
                completion_tokens: None,
                reasoning_tokens: None,
                cache_read_tokens: None,
                cache_write_tokens: None,
                total_tokens: None,
                cost_micro_usd: None,
            }
    );
    Ok(())
}
