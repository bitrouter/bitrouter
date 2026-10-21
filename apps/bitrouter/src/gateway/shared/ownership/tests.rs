//! Real storage interleavings for custody; these do not claim transport bounds.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::execution::ExecutionState;
use bitrouter_ai::types::ModelOperation;
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter, sea_query::Expr};
use serde_json::Value;

use super::{AdmissionPhase, Generation, Registry, Submission, lock};
use crate::auth::{
    db::{NewApiKey, insert_api_key, upsert_user},
    entities::api_keys,
};
use crate::gateway::shared::{
    authority::{Authority, Revision},
    crypto::DeploymentKey,
    engine::{Admission, AdmissionResult, Engine, TenantLimits},
    store::{Kind, NewRecord, Store, Stored},
};

struct Fixture {
    engine: Engine,
    registry: std::sync::Arc<Registry>,
    tenant: String,
    principal: String,
    key_hash: String,
    fingerprint: String,
    revision: Revision,
}

impl Fixture {
    async fn new(store: &Store, limit: usize) -> Result<Self> {
        let authority = Authority::new(store.clone(), DeploymentKey::from_bytes([9; 32])?);
        let revision = authority.head().await?.revision();
        let engine = Engine::join(authority, 30_000).await?;
        let tenant = uuid::Uuid::new_v4().to_string();
        let principal = uuid::Uuid::new_v4().to_string();
        let key_hash = crate::auth::keys::generate().hash;
        upsert_user(store.database(), &tenant).await?;
        insert_api_key(
            store.database(),
            &NewApiKey {
                id: principal.clone(),
                key_hash: key_hash.clone(),
                user_id: tenant.clone(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        engine
            .set_tenant(
                &tenant,
                None,
                &TenantLimits {
                    concurrent_executions: 8,
                    requests_per_minute: 100,
                    resource_pools: Default::default(),
                },
            )
            .await?;
        Ok(Self {
            registry: Registry::new(engine.clone(), limit)?,
            engine,
            tenant,
            principal,
            key_hash,
            fingerprint: "a".repeat(64),
            revision,
        })
    }

    fn request(&self) -> Admission<'_> {
        Admission {
            requirements: crate::gateway::shared::capture::Requirements::fixture(),
            key_hash: &self.key_hash,
            correlation_id: "custody-fixture",
            idempotency_key: None,
            fingerprint: &self.fingerprint,
            operation: ModelOperation::Generation,
            revision: self.revision,
        }
    }

    async fn accepted(&self) -> Result<std::sync::Arc<super::SharedAdmission>> {
        match self.registry.reserve()?.admit(&self.request()).await? {
            Submission::Accepted(admission) => Ok(admission),
            Submission::Existing(_) => anyhow::bail!("unexpected custody duplicate"),
        }
    }
}

pub(in crate::gateway::shared) async fn exercise(first: &Store, second: &Store) -> Result<()> {
    Box::pin(bounds_and_rejection(first)).await?;
    Box::pin(duplicate_and_revocation(first, second)).await?;
    Box::pin(cancelled_finish_requeues(first, second)).await?;
    Box::pin(uncertain_admission_probe(first, second)).await?;
    Box::pin(probe_reads_after_wait(first, second)).await?;
    Box::pin(partial_projection_survives_cancelled_finish(first, second)).await?;
    Ok(())
}

async fn bounds_and_rejection(first: &Store) -> Result<()> {
    let fixture = Fixture::new(first, 2).await?;
    let live = fixture.registry.reserve()?;
    let abandoned = fixture.registry.reserve()?;
    ensure!(
        fixture.registry.reserve().is_err(),
        "admission must not queue"
    );
    ensure!(!fixture.registry.reconcile_one().await?);
    drop(abandoned);
    ensure!(
        fixture.registry.reconcile_one().await?,
        "live front must not starve retired entry"
    );
    ensure!(fixture.registry.pending() == 1);
    let rejected = fixture.registry.reserve()?;
    let invalid = Admission {
        requirements: crate::gateway::shared::capture::Requirements::fixture(),
        key_hash: &"b".repeat(64),
        ..fixture.request()
    };
    ensure!(rejected.admit(&invalid).await.is_err());
    ensure!(!fixture.registry.reconcile_one().await?);
    ensure!(fixture.registry.reconcile_one().await?);
    let quota = first
        .get::<Value>(Kind::Tenant, &fixture.tenant)
        .await?
        .context("tenant missing")?;
    ensure!(quota.value["active"] == 0 && quota.value["accepted"] == 0);
    drop(live);
    ensure!(fixture.registry.reconcile_one().await?);
    ensure!(fixture.registry.pending() == 0);
    Ok(())
}

async fn duplicate_and_revocation(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, 2).await?;
    let request = Admission {
        idempotency_key: Some("custody-duplicate"),
        ..fixture.request()
    };
    let original = match fixture.registry.reserve()?.admit(&request).await? {
        Submission::Accepted(admission) => admission,
        Submission::Existing(_) => anyhow::bail!("first submission duplicated"),
    };
    let id = original.execution_id.clone();
    match fixture.registry.reserve()?.admit(&request).await? {
        Submission::Existing(view) => ensure!(view.execution_id == id),
        Submission::Accepted(_) => anyhow::bail!("duplicate admitted twice"),
    }
    ensure!(!fixture.registry.reconcile_one().await?);
    ensure!(fixture.registry.reconcile_one().await?);
    ensure!(matches!(
        fixture.engine.query(&fixture.key_hash, &id).await?.state,
        ExecutionState::Accepted
    ));
    api_keys::Entity::update_many()
        .col_expr(api_keys::Column::Active, Expr::value(0))
        .filter(api_keys::Column::Id.eq(&fixture.principal))
        .exec(second.database())
        .await?;
    ensure!(fixture.engine.query(&fixture.key_hash, &id).await.is_err());
    drop(original);
    ensure!(fixture.registry.reconcile_one().await?);
    ensure!(matches!(
        fixture.engine.owned_execution(&id).await?.state,
        ExecutionState::Terminal { .. }
    ));
    let quota = first
        .get::<Value>(Kind::Tenant, &fixture.tenant)
        .await?
        .context("tenant missing")?;
    ensure!(quota.value["active"] == 0 && quota.value["accepted"] == 1);
    Ok(())
}

async fn cancelled_finish_requeues(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, 1).await?;
    let generation = Generation::new();
    let admission = match fixture
        .registry
        .reserve_for(generation)?
        .admit(&fixture.request())
        .await?
    {
        Submission::Accepted(admission) => admission,
        Submission::Existing(_) => anyhow::bail!("unexpected duplicate"),
    };
    let id = admission.execution_id.clone();
    drop(admission);
    let tx = second.begin().await?;
    let _: Stored<Value> = second.lock(&tx, Kind::Tenant, &fixture.tenant).await?;
    ensure!(
        tokio::time::timeout(Duration::from_millis(50), fixture.registry.reconcile_one())
            .await
            .is_err()
    );
    ensure!(fixture.registry.pending() == 1 && fixture.registry.reserve().is_err());
    ensure!(fixture.registry.pending_for(generation) == 1);
    ensure!(lock(&fixture.registry.entries).queued.len() == 1);
    tx.rollback().await?;
    ensure!(fixture.registry.reconcile_one().await?);
    ensure!(matches!(
        fixture.engine.owned_execution(&id).await?.state,
        ExecutionState::Terminal { .. }
    ));
    ensure!(fixture.registry.pending() == 0);
    ensure!(fixture.registry.pending_for(generation) == 0);
    let next = fixture.registry.reserve()?;
    drop(next);
    ensure!(fixture.registry.reconcile_one().await?);
    Ok(())
}

async fn uncertain_admission_probe(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, 1).await?;
    let generation = Generation::new();
    let reservation = fixture.registry.reserve_for(generation)?;
    let id = reservation.0.execution_id.clone();
    *lock(&reservation.0.phase) = AdmissionPhase::UnknownAck;
    // Commit without delivering its result to Reservation: the retained stable
    // identity, not a repeated admission, is the recovery input.
    ensure!(matches!(
        fixture
            .engine
            .admit_with_id(&id, &fixture.request())
            .await?,
        AdmissionResult::Accepted(..)
    ));
    let stored = first
        .get::<Value>(Kind::Execution, &id)
        .await?
        .context("execution missing")?;
    let owner = stored.value["owner"].as_str().context("owner missing")?;
    let tx = second.begin().await?;
    let _: Stored<Value> = second.lock_shared(&tx, Kind::Instance, owner).await?;
    drop(reservation);
    ensure!(
        tokio::time::timeout(Duration::from_millis(50), fixture.registry.reconcile_one())
            .await
            .is_err()
    );
    ensure!(lock(&fixture.registry.entries).queued.len() == 1);
    ensure!(fixture.registry.reserve().is_err());
    ensure!(fixture.registry.pending_for(generation) == 1);
    tx.rollback().await?;
    ensure!(fixture.registry.reconcile_one().await?);
    ensure!(fixture.registry.pending_for(generation) == 0);
    ensure!(matches!(
        fixture.engine.owned_execution(&id).await?.state,
        ExecutionState::Terminal { .. }
    ));

    let absent = fixture.registry.reserve()?;
    let absent_id = absent.0.execution_id.clone();
    *lock(&absent.0.phase) = AdmissionPhase::UnknownAck;
    drop(absent);
    ensure!(fixture.registry.reconcile_one().await?);
    ensure!(fixture.engine.owned_execution(&absent_id).await.is_err());
    let quota = first
        .get::<Value>(Kind::Tenant, &fixture.tenant)
        .await?
        .context("tenant missing")?;
    ensure!(quota.value["active"] == 0 && quota.value["accepted"] == 1);
    Ok(())
}

async fn probe_reads_after_wait(first: &Store, second: &Store) -> Result<()> {
    let fixture = Fixture::new(first, 1).await?;
    let admission = fixture.accepted().await?;
    let original_id = admission.execution_id.clone();
    let mut body = first
        .get::<Value>(Kind::Execution, &original_id)
        .await?
        .context("execution missing")?
        .value;
    let owner = body["owner"].as_str().context("owner missing")?.to_owned();
    let id = uuid::Uuid::new_v4().to_string();
    body["view"]["execution_id"] = Value::String(id.clone());
    // Only the probe's SQL snapshot is under test. This synthetic row is never
    // dispatched or finalized and does not pretend to own a tenant reservation.
    let tx = second.begin().await?;
    let _: Stored<Value> = second.lock_shared(&tx, Kind::Instance, &owner).await?;
    let now = Store::now(&tx).await?;
    ensure!(
        second
            .create(
                &tx,
                NewRecord {
                    kind: Kind::Execution,
                    key: &id,
                    status: "accepted",
                    due_at_ms: None,
                    now_ms: now,
                    value: &body,
                }
            )
            .await?
    );
    let mut probe = Box::pin(fixture.engine.quiescent_execution(&id));
    ensure!(
        tokio::time::timeout(Duration::from_millis(50), &mut probe)
            .await
            .is_err()
    );
    tx.commit().await?;
    ensure!(
        probe
            .await?
            .context("post-wait commit was invisible")?
            .execution_id
            == id
    );
    drop(admission);
    ensure!(fixture.registry.reconcile_one().await?);
    Ok(())
}

/// A selected policy survives cache removal, a changed-contract duplicate and a
/// cancelled finalization, even though no provider attempt was ever prepared.
async fn partial_projection_survives_cancelled_finish(first: &Store, second: &Store) -> Result<()> {
    for variant in ["cost", "unavailable"] {
        Box::pin(partial_projection_variant(first, second, variant)).await?;
    }
    Ok(())
}

async fn partial_projection_variant(first: &Store, second: &Store, variant: &str) -> Result<()> {
    use crate::gateway::caller::CallerContext;
    use crate::gateway::observability::decision::{EvalInvocation, PendingEvalDecisionStore};
    use crate::gateway::pipeline::{
        context::PipelineContext, routing::ModelSelector, types::PipelineRequest,
    };
    use crate::gateway::shared::capture::Requirements;
    use crate::policy_lock::{PolicyLock, PolicyRuntime, semantic_digest};
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    let fixture = Fixture::new(first, 2).await?;
    let config = crate::config::parse(
        "inherit_defaults: false\npresets:\n  coding:\n    model: fixture:strong\n    policy: auto\n",
    )?;
    let mut document: PolicyLock =
        serde_saphyr::from_str(include_str!("../../../../policy/default.yaml"))?;
    document.policies.insert(
        "auto:cost".into(),
        document
            .policies
            .get("auto")
            .context("missing auto policy")?
            .clone(),
    );
    let pending = PendingEvalDecisionStore::default();
    let runtime =
        PolicyRuntime::from_document(&config, Some(document.clone()), pending.clone(), None)?;
    let request = Admission {
        requirements: Requirements::from_config(&config, Some(&document))?,
        idempotency_key: Some("original-contract"),
        ..fixture.request()
    };
    let admission = match fixture.registry.reserve()?.admit(&request).await? {
        Submission::Accepted(admission) => admission,
        Submission::Existing(_) => anyhow::bail!("unexpected duplicate"),
    };
    let id = admission.execution_id.clone();
    let mut pipeline = PipelineRequest::new(
        "fixture",
        CallerContext::anonymous(),
        bitrouter_ai::types::Prompt {
            model: "fixture:strong".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![],
            tools: vec![],
            params: Default::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        },
    );
    admission.attach(&mut pipeline);
    let mut context = PipelineContext::new(pipeline);
    runtime
        .prepare_selection("auto", Some(variant), &mut context)
        .await?;
    runtime
        .select_variant("auto", Some(variant), &mut context)
        .await?;
    let invocation = context
        .extension::<EvalInvocation>()
        .context("missing policy invocation")?;
    ensure!(
        pending.remove(&invocation, &fixture.tenant),
        "fixture did not select an evaluable tier"
    );
    let retained = {
        let entries = lock(&fixture.registry.entries);
        entries
            .owned
            .get(&id)
            .context("missing custody")?
            .capture
            .snapshot()
            .context("missing capture")?
    };
    let mut expected = serde_json::to_value(retained.as_ref())?;
    ensure!(expected["policy"]["state"] == "evaluated");
    ensure!(
        expected["policy"]["policy"]
            == if variant == "cost" {
                "auto:cost"
            } else {
                "auto"
            }
    );
    ensure!(expected["policy"]["digest"] == semantic_digest(&document)?);
    ensure!(expected["phase"]["state"] == "preparing");
    ensure!(expected["policy"]["treatment"]["decision"]["selected_tier"].is_string());

    // The same idempotency binding returns its original authority even when a
    // later candidate would have a different sink contract.
    let mut replacement = config.clone();
    replacement.trajectory.enabled = true;
    let duplicate = Admission {
        requirements: Requirements::from_config(&replacement, Some(&document))?,
        ..request
    };
    ensure!(
        matches!(fixture.registry.reserve()?.admit(&duplicate).await?, Submission::Existing(view) if view.execution_id == id)
    );
    ensure!(!fixture.registry.reconcile_one().await?);
    ensure!(fixture.registry.reconcile_one().await?);
    let before = second
        .get::<Value>(Kind::Execution, &id)
        .await?
        .context("missing execution")?
        .value;
    ensure!(before["projection"]["requirements"]["trajectory"] == "disabled");
    ensure!(before["projection"]["policy"]["state"] == "pending");
    drop(context);
    drop(admission);
    drop(runtime);

    let blocker = second.begin().await?;
    let _: Stored<Value> = second.lock(&blocker, Kind::Tenant, &fixture.tenant).await?;
    let pid: i32 = blocker
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid",
        ))
        .await?
        .context("missing pid")?
        .try_get("", "pid")?;
    let mut finishing = Box::pin(fixture.registry.reconcile_one());
    let waiting = async {
        loop {
            // https://www.postgresql.org/docs/current/functions-info.html
            let blocked: bool = second.database().query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))) AS waiting", [pid.into()]
            )).await?.context("missing blocker result")?.try_get("", "waiting")?;
            if blocked {
                return Ok::<(), anyhow::Error>(());
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    };
    tokio::select! {
        result = &mut finishing => anyhow::bail!("finish completed before observed wait: {result:?}"),
        result = tokio::time::timeout(Duration::from_secs(1), waiting) => result??,
    }
    drop(finishing);
    ensure!(fixture.registry.pending() == 1);
    blocker.rollback().await?;
    ensure!(fixture.registry.reconcile_one().await?);
    ensure!(fixture.registry.pending() == 0);
    expected["phase"] = serde_json::json!({"state":"terminal","route_ready":false});
    let after = second
        .get::<Value>(Kind::Execution, &id)
        .await?
        .context("missing finalized execution")?
        .value;
    ensure!(
        after["projection"] == expected,
        "cancelled finish lost or recomputed captured policy"
    );
    ensure!(
        after["view"]["attempts"]
            .as_array()
            .is_some_and(Vec::is_empty)
    );
    ensure!(matches!(
        fixture.engine.owned_execution(&id).await?.state,
        ExecutionState::Terminal { .. }
    ));
    Ok(())
}
