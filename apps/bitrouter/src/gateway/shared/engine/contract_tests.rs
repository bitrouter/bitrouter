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
    Admission, AdmissionResult, Engine, EpisodeFence, Execution, Outbox, Resource, Tenant,
    TenantLimits, TrajectoryBinding,
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
use crate::trajectory::{
    shared::SharedTrajectory,
    types::{
        BeginRequest, EpisodeStart, HistoryCompleteness, KeyedDigest, TRAJECTORY_SCHEMA_VERSION,
        TrajectoryEvent, TrajectoryEventKind, TrajectoryEvidence,
    },
};

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
            format: 1,
            routing: json!({"trajectory": {"enabled": true}}),
            policies: vec![],
            policy_lock: Some(crate::model_policy::parse(&json!({
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
                    self.publication.policy_lock.as_ref(),
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
async fn postgres_legacy_publication_rejection_preserves_authority() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(&mut database).await?;
        let head = f.engine.authority.head().await?;
        let before = f.snapshot().await?;
        let mut rejected = f.publication.clone();
        let mut legacy: crate::policy_lock::PolicyLock =
            serde_saphyr::from_str(include_str!("../../../../policy/default.yaml"))?;
        legacy
            .policies
            .get_mut("auto")
            .context("legacy policy missing")?
            .progress_guard = Some(crate::trajectory::guard::ProgressGuardPolicy {
            escalation_tier: "strong".into(),
            protected_tiers: ["strong".into()].into(),
            max_consecutive_unprotected: None,
            max_same_projection_unprotected: None,
            max_recovery_count: None,
            max_episode_requests: Some(1),
            max_episode_elapsed_ms: None,
            max_episode_cost_micro_usd: None,
            hold_for_requests: 2,
            incomplete_history: crate::trajectory::guard::IncompleteHistoryAction::Observe,
        });
        crate::policy_lock::validate_document(&legacy)?;
        rejected.policy_lock = Some(legacy);
        let error = f
            .engine
            .authority
            .publish(head.config_generation, &rejected)
            .await
            .err()
            .context("legacy publication accepted")?;
        ensure!(error.to_string().contains("model-only contract"));
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
async fn postgres_read_committed_terminal_invalidates_serializable_fence_snapshot() -> Result<()> {
    let mut database = EmptyDatabase::create().await?;
    let result = tokio::time::timeout(Duration::from_secs(30), async {
        let f = Fixture::new(&mut database).await?;
        let id = f.admit().await?;
        // Exercise the ordinary binding/finalization primitives directly. There
        // is no guarded route, escalation or experimental policy publication.
        let binding = TrajectoryBinding {
            owner_user_id: "tenant".into(),
            episode_id: "ordinary-episode".into(),
            request_id: "ordinary-request".into(),
            correlation_epoch: "ordinary-epoch".into(),
        };
        let setup = f.observer.begin().await?;
        let digest = KeyedDigest::parse(format!("hmac-sha256:ordinary-epoch:{}", "a".repeat(64)))?;
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: format!("start-{id}"),
            owner_user_id: binding.owner_user_id.clone(),
            episode_id: binding.episode_id.clone(),
            request_id: Some(binding.request_id.clone()),
            sequence: 1,
            kind: TrajectoryEventKind::RequestStarted,
            evidence: TrajectoryEvidence {
                structural: Default::default(),
                categorical: [
                    ("correlation.source".into(), "explicit_root".into()),
                    ("history.completeness".into(), "complete".into()),
                ]
                .into(),
                digests: Default::default(),
                route_measurement: None,
                routing_evidence: vec![],
            },
            captured_at: "2026-01-01T00:00:00Z".into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        SharedTrajectory::new(f.observer.namespace())?
            .begin_in(
                &setup,
                "tenant",
                &BeginRequest {
                    episode: EpisodeStart {
                        episode_id: binding.episode_id.clone(),
                        correlation_digest: digest.clone(),
                        correlation_key_id: binding.correlation_epoch.clone(),
                        correlation_source: "explicit_root".into(),
                        completeness: HistoryCompleteness::Complete,
                    },
                    event,
                    full_input_digest: digest,
                    native_parent_id: None,
                    protocol: "responses".into(),
                },
            )
            .await?;
        f.engine.bind_trajectory(&setup, &id, &binding).await?;
        let fence = f
            .engine
            .episode_fence(&setup, &binding, Store::now(&setup).await?)
            .await?;
        let key = fence.key().to_owned();
        setup.commit().await?;
        let stale = f.observer.begin().await?;
        let result = async {
            // This awaited MVCC read fixes the old snapshot without holding a
            // row lock that could block the Read Committed terminal writer.
            let before: Stored<EpisodeFence> = f
                .observer
                .read(&stale, Kind::EpisodeFence, &key)
                .await?
                .context("original fence missing")?;
            ensure!(before.value.pending.is_empty());
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
            ensure!(execution.value.trajectory.as_ref() == Some(&binding));
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
                .engine
                .episode_fence(&stale, &binding, Store::now(&stale).await?)
                .await
                .err()
                .context("stale terminal fence remained lockable")?;
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
        let pending: Stored<EpisodeFence> = f
            .observer
            .get(Kind::EpisodeFence, &key)
            .await?
            .context("pending fence missing")?;
        ensure!(pending.value.pending == [id].into());
        ensure!(pending.revision()? == fence.revision()? + 1);
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
