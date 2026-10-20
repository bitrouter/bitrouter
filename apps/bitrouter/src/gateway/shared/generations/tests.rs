//! Complete assembly and durable producer ownership against PostgreSQL.

use std::time::Duration;

use axum::body::Body;
use http::{Request, StatusCode};
use sea_orm::{
    ConnectionTrait as _, DatabaseBackend, DatabaseConnection, DatabaseTransaction, Statement,
};
use tower::ServiceExt as _;

use super::*;
use crate::gateway::shared::{
    crypto::DeploymentKey,
    publication::Configuration,
    schema,
    store::{self, Kind, Store},
};

struct Fixture {
    db: sea_orm::DatabaseConnection,
    authority: Authority,
    config: Config,
    manager: Generations,
}

async fn observe_blocked_query(
    database: &DatabaseConnection,
    blocker: &DatabaseTransaction,
) -> Result<()> {
    let pid: i32 = blocker
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid",
        ))
        .await?
        .context("blocker session missing")?
        .try_get("", "pid")?;
    tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            // Observe the actual lock wait, rather than infer it from elapsed time.
            // https://www.postgresql.org/docs/current/functions-info.html#FUNCTIONS-INFO-SESSION-TABLE
            let row = database.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY (pg_blocking_pids(pid))) AS waiting", [pid.into()])).await?.context("lock observation missing")?;
            if row.try_get::<bool>("", "waiting")? { return Ok::<(), anyhow::Error>(()); }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    }).await.context("generation operation never reached the held row lock")?
}

impl Fixture {
    async fn new(limit: usize) -> Result<Self> {
        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
        let db = store::connect(&url, 4, Duration::from_secs(3)).await?;
        crate::db::run_migrations(&db).await?;
        let namespace = format!("generation-{}", uuid::Uuid::new_v4());
        let store = Store::new(db.clone(), &namespace, Duration::from_secs(3))?;
        schema::initialize(&store).await?;
        let authority = Authority::new(store, DeploymentKey::from_bytes(rand::random())?);
        authority.initialize().await?;
        let mut config = crate::config::parse_with(
            "inherit_defaults: false\nregistry:\n  enabled: false\n",
            |_| None,
        )?;
        config.database.url = url;
        config.gateway_state = serde_json::from_value(serde_json::json!({
            "mode":"shared", "namespace":namespace, "deployment_key_env":"UNUSED_GENERATION_FIXTURE_KEY", "max_retired":limit
        }))?;
        let GatewayStateConfig::Shared(shared) = &config.gateway_state else {
            anyhow::bail!("shared fixture missing");
        };
        let engine = Engine::join(authority.clone(), 300_000).await?;
        let mut mismatched = shared.clone();
        mismatched.namespace.push_str("-other");
        ensure!(
            Generations::new(engine.clone(), &mismatched, Arc::new(Telemetry::new(None)?)).is_err()
        );
        let manager = Generations::new(engine, shared, Arc::new(Telemetry::new(None)?))?;
        Ok(Self {
            db,
            authority,
            config,
            manager,
        })
    }

    async fn next(&self) -> Result<Input> {
        let head = self.authority.head().await?;
        self.authority
            .publish(
                head.config_generation,
                &Configuration {
                    format: 1,
                    routing: serde_json::json!({}),
                    policies: Vec::new(),
                    policy_lock: None,
                    credentials: Vec::new(),
                    require_known_pricing: false,
                },
            )
            .await?;
        self.same().await
    }

    async fn same(&self) -> Result<Input> {
        let head = self.authority.head().await?;
        let keys = self.authority.keys(&head).await?;
        Ok(Input {
            revision: head.revision(),
            config: self.config.clone(),
            authority: self.authority.clone(),
            continuation_key: keys.continuation()?,
            correlation_key: keys.active_correlation()?,
            auth_appliers: AuthAppliers::new(),
            policies: Vec::new(),
            policy_lock: None,
        })
    }

    async fn build(&self, input: Input) -> Result<Generation> {
        let id = self.manager.start_candidate(input)?;
        let candidate = self
            .manager
            .wait_candidate(Instant::now() + Duration::from_secs(5))
            .await?;
        ensure!(
            candidate.generation == id
                && candidate.joined
                && candidate.ready
                && candidate.failure.is_none()
        );
        Ok(id)
    }

    async fn close(self, result: Result<()>) -> Result<()> {
        self.manager.withdraw();
        let ids: Vec<_> = {
            let state = self.manager.lock();
            state
                .current
                .iter()
                .chain(state.retired.iter())
                .chain(state.candidate.iter())
                .map(|resource| resource.id)
                .collect()
        };
        let cleanup = async {
            for id in ids {
                let attempt = self
                    .manager
                    .begin_retirement(id, Instant::now() + Duration::from_secs(5))?;
                ensure!(
                    self.manager.retire(id, attempt).await?.reclaimed,
                    "fixture retained resources"
                );
            }
            Ok::<(), anyhow::Error>(())
        }
        .await;
        drop(self.manager);
        let closed = self.db.close().await;
        result?;
        cleanup?;
        closed?;
        Ok(())
    }

    async fn close_forced(self, result: Result<()>) -> Result<()> {
        self.manager.request_force();
        let cleanup = async {
            self.manager.force_pipelines(None)?;
            let reports =
                tokio::time::timeout(Duration::from_secs(5), self.manager.join_forced()).await??;
            ensure!(
                reports.iter().all(|report| report.is_quiescent()),
                "forced fixture retains local roots"
            );
            Ok::<(), anyhow::Error>(())
        }
        .await;
        drop(self.manager);
        let closed = self.db.close().await;
        result?;
        cleanup?;
        closed?;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn whole_resources_and_candidate_survive_the_retired_budget() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let first = fixture.build(fixture.next().await?).await?;
        fixture.manager.publish_candidate()?;
        fixture.manager.serving().set_ready(true);
        let components = fixture
            .manager
            .retained(first)?
            .components()
            .context("first components absent")?;
        let whole = Arc::downgrade(&components);
        let extra_snapshot = components
            .snapshot
            .as_ref()
            .context("first snapshot absent")?
            .clone();
        drop(components);
        let (snapshot, reservation, source) = fixture.manager.serving().acquire_request()?;
        let second = fixture.build(fixture.next().await?).await?;
        fixture.manager.publish_candidate()?;
        let third = fixture.build(fixture.next().await?).await?;
        ensure!(fixture.manager.publish_candidate().is_err());
        ensure!(
            fixture
                .manager
                .start_candidate(fixture.same().await?)
                .is_err()
        );
        ensure!(fixture.manager.retained_count() == 3);
        ensure!(fixture.manager.serving().snapshot()?.owner == second);
        let expired = fixture.manager.begin_retirement(first, Instant::now())?;
        ensure!(
            !fixture.manager.retire(first, expired).await?.reclaimed && whole.upgrade().is_some()
        );
        drop((snapshot, reservation, source));
        // Snapshot references matter independently from source/custody counts.
        let attempt = fixture
            .manager
            .begin_retirement(first, Instant::now() + Duration::from_secs(5))?;
        let retained = fixture.manager.retire(first, attempt).await?;
        ensure!(!retained.reclaimed && retained.producer_closed && retained.custody == 0);
        ensure!(whole.upgrade().is_some());
        drop(extra_snapshot);
        let reclaimed = fixture.manager.retire(first, attempt).await?;
        ensure!(reclaimed.is_clean() && whole.upgrade().is_none());
        ensure!(fixture.manager.retire(first, attempt).await?.reclaimed);
        ensure!(fixture.manager.publish_candidate()? == third);
        fixture.manager.withdraw();
        fixture.manager.serving().set_ready(true);
        ensure!(fixture.manager.serving().loaded_revision().is_none());
        ensure!(fixture.manager.serving().acquire_request().is_err());
        for path in ["/health", "/metrics", "/v1/models"] {
            let response = fixture
                .manager
                .serving()
                .router()
                .oneshot(Request::builder().uri(path).body(Body::empty())?)
                .await?;
            ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
        }
        ensure!(
            fixture
                .manager
                .start_candidate(fixture.same().await?)
                .is_err()
        );
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelled_build_wait_retains_its_handle_and_single_slot() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let input = fixture.next().await?;
        let tx = fixture.authority.store().begin().await?;
        fixture
            .authority
            .store()
            .lock::<super::super::authority::Head>(&tx, Kind::Authority, "head")
            .await?;
        let id = fixture.manager.start_candidate(input)?;
        let resource = fixture.manager.retained(id)?;
        let task_id = resource.build_task_id();
        ensure!(task_id.is_some());
        let mut waiter = Box::pin(
            fixture
                .manager
                .wait_candidate(Instant::now() + Duration::from_secs(5)),
        );
        ensure!(futures::poll!(waiter.as_mut()).is_pending());
        ensure!(
            fixture
                .manager
                .wait_candidate(Instant::now())
                .await
                .is_err()
        );
        drop(waiter);
        let timed = fixture
            .manager
            .wait_candidate(Instant::now() + Duration::from_millis(20))
            .await?;
        ensure!(!timed.joined && resource.build_task_id() == task_id);
        fixture.manager.discard_candidate()?;
        let expired = fixture.manager.begin_retirement(id, Instant::now())?;
        ensure!(!fixture.manager.retire(id, expired).await?.reclaimed);
        ensure!(resource.build_task_id() == task_id && fixture.manager.retained_count() == 1);
        tx.rollback().await?;
        let joined = fixture
            .manager
            .wait_candidate(Instant::now() + Duration::from_secs(5))
            .await?;
        ensure!(joined.joined && !joined.ready && joined.failure.is_none());
        ensure!(fixture.manager.publish_candidate().is_err());
        drop(resource);
        let attempt = fixture
            .manager
            .begin_retirement(id, Instant::now() + Duration::from_secs(5))?;
        ensure!(fixture.manager.retire(id, attempt).await?.is_clean());
        ensure!(fixture.manager.retained_count() == 0);
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn failed_and_equal_revision_candidates_close_only_their_own_producer() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let current = fixture.build(fixture.next().await?).await?;
        fixture.manager.publish_candidate()?;
        let duplicate = fixture.build(fixture.same().await?).await?;
        let live = fixture.manager.retained(current)?;
        let other = fixture.manager.retained(duplicate)?;
        ensure!(live.producer_id() != other.producer_id());
        ensure!(fixture.manager.publish_candidate().is_err());
        fixture.manager.discard_candidate()?;
        let attempt = fixture
            .manager
            .begin_retirement(duplicate, Instant::now() + Duration::from_secs(5))?;
        ensure!(fixture.manager.retire(duplicate, attempt).await?.is_clean());
        let row = fixture
            .authority
            .store()
            .get::<serde_json::Value>(Kind::ContinuationProducer, live.producer_id())
            .await?
            .context("live producer missing")?;
        ensure!(row.value["closed_at_us"].is_null());
        drop((live, other));
        let mut input = fixture.same().await?;
        input.config.server_tools.advisor = true;
        let failed = fixture.manager.start_candidate(input)?;
        let status = fixture
            .manager
            .wait_candidate(Instant::now() + Duration::from_secs(5))
            .await?;
        ensure!(status.joined && status.failure == Some(BuildFailure::Assembly));
        fixture.manager.discard_candidate()?;
        let attempt = fixture
            .manager
            .begin_retirement(failed, Instant::now() + Duration::from_secs(5))?;
        let report = fixture.manager.retire(failed, attempt).await?;
        ensure!(report.reclaimed && report.producer_closed && !report.is_clean());
        // A candidate that cannot authenticate its continuation key must still
        // close its own retained registration intent under deployment authority.
        let mut input = fixture.same().await?;
        input.continuation_key = ContinuationKey::from_bytes(rand::random())?;
        let failed = fixture.manager.start_candidate(input)?;
        let status = fixture
            .manager
            .wait_candidate(Instant::now() + Duration::from_secs(5))
            .await?;
        ensure!(status.joined && status.failure == Some(BuildFailure::Assembly));
        fixture.manager.discard_candidate()?;
        let attempt = fixture
            .manager
            .begin_retirement(failed, Instant::now() + Duration::from_secs(5))?;
        let report = fixture.manager.retire(failed, attempt).await?;
        ensure!(report.reclaimed && report.producer_closed && !report.is_clean());
        let live = fixture.manager.retained(current)?;
        let row = fixture
            .authority
            .store()
            .get::<serde_json::Value>(Kind::ContinuationProducer, live.producer_id())
            .await?
            .context("live producer missing")?;
        ensure!(row.value["closed_at_us"].is_null());
        ensure!(fixture.manager.serving().snapshot()?.owner == current);
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn history_errors_do_not_prevent_confirmed_reclamation() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let id = fixture.build(fixture.next().await?).await?;
        let components = fixture
            .manager
            .retained(id)?
            .components()
            .context("components absent")?;
        let tasks = components
            .snapshot
            .as_ref()
            .context("snapshot absent")?
            .tasks();
        tasks.record_finalizer_failure();
        tasks.spawn_execution(async {
            std::panic::resume_unwind(Box::new("owned fixture panic"));
        });
        fixture.manager.discard_candidate()?;
        let attempt = fixture
            .manager
            .begin_retirement(id, Instant::now() + Duration::from_secs(5))?;
        drop(components);
        let report = fixture.manager.retire(id, attempt).await?;
        let summary = report.pipeline.context("joined pipeline missing")?;
        ensure!(
            report.reclaimed
                && !report.is_clean()
                && summary.panicked == 1
                && summary.finalizer_failures == 1
        );
        ensure!(
            report
                .native
                .context("native barrier missing")?
                .worker
                .joined
        );
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn retirement_sweeps_past_busy_batches_and_drains_a_large_backlog() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let old = fixture.build(fixture.next().await?).await?;
        fixture.manager.publish_candidate()?;
        let serving = fixture.manager.serving();
        let held_old = serving.custody.reserve_for(old)?;
        let current = fixture.build(fixture.next().await?).await?;
        fixture.manager.publish_candidate()?;
        let held_current = (0..40)
            .map(|_| serving.custody.reserve_for(current))
            .collect::<Result<Vec<_>>>()?;
        // Rotate the old live entry behind forty live current entries.
        ensure!(!serving.custody.reconcile_one().await?);
        drop(held_old);
        for _ in 0..65 {
            drop(serving.custody.reserve_for(old)?);
        }
        ensure!(serving.custody.pending_for(old) == 66);
        let attempt = fixture
            .manager
            .begin_retirement(old, Instant::now() + Duration::from_secs(5))?;
        ensure!(fixture.manager.retire(old, attempt).await?.is_clean());
        ensure!(serving.custody.pending_for(current) == 40);
        ensure!(fixture.manager.retained_count() == 1);
        drop(held_current);
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn all_live_custody_returns_after_one_idle_sweep() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let id = fixture.build(fixture.next().await?).await?;
        fixture.manager.discard_candidate()?;
        let serving = fixture.manager.serving();
        let held = (0..65)
            .map(|_| serving.custody.reserve_for(id))
            .collect::<Result<Vec<_>>>()?;
        let attempt = fixture
            .manager
            .begin_retirement(id, Instant::now() + Duration::from_secs(5))?;
        let report =
            tokio::time::timeout(Duration::from_secs(1), fixture.manager.retire(id, attempt))
                .await??;
        ensure!(!report.reclaimed && report.producer_closed && report.custody == 65);
        drop(held);
        ensure!(fixture.manager.retire(id, attempt).await?.is_clean());
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelled_native_close_retains_the_original_deadline_and_owner() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let id = fixture.build(fixture.next().await?).await?;
        fixture.manager.discard_candidate()?;
        let resource = fixture.manager.retained(id)?;
        let producer = resource.producer_id().to_owned();
        let whole = Arc::downgrade(&resource.components().context("components absent")?);
        drop(resource);
        let tx = fixture.authority.store().begin().await?;
        fixture
            .authority
            .store()
            .lock::<super::super::authority::Head>(&tx, Kind::Authority, "head")
            .await?;
        let deadline = Instant::now() + Duration::from_millis(100);
        let attempt = fixture.manager.begin_retirement(id, deadline)?;
        // The outer cancellation leaves D and the complete resource retained.
        ensure!(
            tokio::time::timeout(
                Duration::from_millis(20),
                fixture.manager.retire(id, attempt)
            )
            .await
            .is_err()
        );
        ensure!(whole.upgrade().is_some() && fixture.manager.retained_count() == 1);
        tokio::time::sleep_until(deadline).await;
        tx.rollback().await?;
        let retained = fixture.manager.retire(id, attempt).await?;
        ensure!(!retained.reclaimed && !retained.producer_closed);
        let row = fixture
            .authority
            .store()
            .get::<serde_json::Value>(Kind::ContinuationProducer, &producer)
            .await?
            .context("producer missing")?;
        ensure!(row.value["closed_at_us"].is_null());
        let retry = fixture
            .manager
            .begin_retirement(id, Instant::now() + Duration::from_secs(5))?;
        ensure!(fixture.manager.retire(id, retry).await?.is_clean());
        ensure!(whole.upgrade().is_none());
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn force_broadcast_preserves_all_slots_and_unresolved_custody() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        ensure!(fixture.manager.join_forced().await.is_err());
        let old = fixture.build(fixture.next().await?).await?;
        fixture.manager.publish_candidate()?;
        let current = fixture.build(fixture.next().await?).await?;
        fixture.manager.publish_candidate()?;
        drop(fixture.manager.serving().custody.reserve_for(old)?);
        let input = fixture.next().await?;
        let tx = fixture.authority.store().begin().await?;
        fixture
            .authority
            .store()
            .lock::<super::super::authority::Head>(&tx, Kind::Authority, "head")
            .await?;
        let candidate = fixture.manager.start_candidate(input)?;
        ensure!(
            !fixture
                .manager
                .wait_candidate(Instant::now() + Duration::from_millis(20))
                .await?
                .joined
        );
        observe_blocked_query(&fixture.db, &tx).await?;
        fixture.manager.request_force();
        ensure!(fixture.manager.retained_count() == 3);
        ensure!(
            fixture
                .manager
                .begin_retirement(old, Instant::now())
                .is_err()
        );
        for id in [old, current] {
            let resource = fixture.manager.retained(id)?;
            let components = resource.components().context("components missing")?;
            ensure!(
                components
                    .assembled
                    .continuation_registry
                    .owned_status()?
                    .worker
                    .forced
            );
        }
        // Native/build joins can progress while the external-producer barrier
        // remains closed. Cancelling this wait loses no owner or registered id.
        let mut waiting = Box::pin(fixture.manager.join_forced());
        ensure!(futures::poll!(waiting.as_mut()).is_pending());
        drop(waiting);
        ensure!(fixture.manager.retained_count() == 3);
        fixture.manager.force_pipelines(None)?;
        let reports =
            tokio::time::timeout(Duration::from_secs(1), fixture.manager.join_forced()).await??;
        ensure!(reports.len() == 3 && reports.iter().all(|report| report.is_quiescent()));
        ensure!(
            reports
                .iter()
                .all(|report| !report.native_failed && !report.pipeline_failed)
        );
        let old_report = reports
            .iter()
            .find(|report| report.generation == old)
            .context("old report missing")?;
        ensure!(old_report.custody == 1 && !old_report.durable_resolved());
        let candidate_report = reports
            .iter()
            .find(|report| report.generation == candidate)
            .context("candidate report missing")?;
        ensure!(
            candidate_report.build_joined
                && !candidate_report.has_components
                && !candidate_report.producer_closed
        );
        tx.rollback().await?;
        for id in [old, current] {
            let resource = fixture.manager.retained(id)?;
            let row = fixture
                .authority
                .store()
                .get::<serde_json::Value>(Kind::ContinuationProducer, resource.producer_id())
                .await?
                .context("producer missing")?;
            ensure!(
                row.value["closed_at_us"].is_null(),
                "force started database compensation"
            );
        }
        ensure!(
            fixture.manager.retained_count() == 3
                && fixture.manager.serving().custody.pending_for(old) == 1
        );
        Ok(())
    }
    .await;
    fixture.close_forced(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn force_harvests_a_completed_unobserved_build() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let id = fixture.manager.start_candidate(fixture.next().await?)?;
        let resource = fixture.manager.retained(id)?;
        tokio::time::timeout(Duration::from_secs(5), async {
            while !resource.build_finished() {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await?;
        ensure!(resource.components().is_none());
        fixture.manager.request_force();
        fixture.manager.force_pipelines(None)?;
        let reports = fixture.manager.join_forced().await?;
        let report = reports.first().context("report missing")?;
        ensure!(report.generation == id && report.has_components && report.build_failure.is_none());
        ensure!(report.is_quiescent() && !report.producer_closed && !report.durable_resolved());
        ensure!(report.pipeline.is_some_and(|summary| summary.forced));
        ensure!(report.native.is_some_and(|native| native.worker.forced));
        ensure!(resource.components().is_some() && fixture.manager.publish_candidate().is_err());
        Ok(())
    }
    .await;
    fixture.close_forced(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn force_requires_actual_transport_join_and_reports_retained_sources() -> Result<()> {
    use crate::gateway::shared::transport::{Limits, owned::Transport};
    let fixture = Fixture::new(1).await?;
    let GatewayStateConfig::Shared(config) = &fixture.config.gateway_state else {
        anyhow::bail!("shared config missing");
    };
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let transport = Transport::start(listener, axum::Router::new(), Limits::from(config))?;
    let result = async {
        let id = fixture.build(fixture.next().await?).await?;
        fixture.manager.publish_candidate()?;
        fixture.manager.serving().set_ready(true);
        let (snapshot, reservation, source) = fixture.manager.serving().acquire_request()?;
        fixture.manager.request_force();
        ensure!(fixture.manager.force_pipelines(Some(&transport)).is_err());
        transport.request_force();
        ensure!(transport.join_forced().await?.is_quiescent());
        fixture.manager.force_pipelines(Some(&transport))?;
        let retained = fixture.manager.join_forced().await?;
        ensure!(retained[0].generation == id && !retained[0].is_quiescent());
        ensure!(
            retained[0]
                .pending
                .is_some_and(|pending| pending.sources == 1)
        );
        ensure!(retained[0].pipeline_failed && retained[0].snapshot_readers == 1);
        drop((snapshot, reservation, source));
        let joined = fixture.manager.join_forced().await?;
        ensure!(
            joined[0].is_quiescent() && joined[0].pipeline_failed && !joined[0].durable_resolved()
        );
        Ok(())
    }
    .await;
    transport.request_force();
    let transport_result = transport.join_forced().await;
    drop(transport);
    let result = result.and_then(|()| transport_result.map(|_| ()));
    fixture.close_forced(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn force_interrupts_graceful_sql_without_restarting_cleanup() -> Result<()> {
    let fixture = Fixture::new(1).await?;
    let result = async {
        let id = fixture.build(fixture.next().await?).await?;
        let resource = fixture.manager.retained(id)?;
        let producer = resource.producer_id().to_owned();
        drop(resource);
        fixture.manager.discard_candidate()?;
        let tx = fixture.authority.store().begin().await?;
        fixture
            .authority
            .store()
            .lock::<super::super::authority::Head>(&tx, Kind::Authority, "head")
            .await?;
        let attempt = fixture
            .manager
            .begin_retirement(id, Instant::now() + Duration::from_secs(5))?;
        let mut graceful = Box::pin(fixture.manager.retire(id, attempt));
        tokio::select! {
            result = &mut graceful => anyhow::bail!("blocked grace completed: {result:?}"),
            observed = observe_blocked_query(&fixture.db, &tx) => observed?,
        }
        fixture.manager.request_force();
        ensure!(
            !tokio::time::timeout(Duration::from_secs(1), graceful)
                .await??
                .reclaimed
        );
        fixture.manager.force_pipelines(None)?;
        let reports =
            tokio::time::timeout(Duration::from_secs(1), fixture.manager.join_forced()).await??;
        ensure!(reports[0].is_quiescent() && !reports[0].producer_closed);
        tx.rollback().await?;
        let row = fixture
            .authority
            .store()
            .get::<serde_json::Value>(Kind::ContinuationProducer, &producer)
            .await?
            .context("producer missing")?;
        ensure!(row.value["closed_at_us"].is_null());
        Ok(())
    }
    .await;
    fixture.close_forced(result).await
}
