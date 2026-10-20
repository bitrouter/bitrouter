//! Runtime orchestration against a real shared database and retained sockets.

use super::*;
use anyhow::Context as _;
use axum::body::Body;
use axum::{Router, routing::get};
use http::{Request, StatusCode};
use sea_orm::{ConnectionTrait as _, DatabaseBackend, DatabaseConnection, Statement};
use tokio::net::TcpListener;
use tokio::sync::Notify;
use tower::ServiceExt as _;

use crate::config::shared_process::{Process, Server};
use crate::gateway::shared::{
    authority::Authority,
    crypto::DeploymentKey,
    publication::Configuration,
    schema,
    store::{self, Store},
};

struct Fixture {
    main: DatabaseConnection,
    heartbeat: DatabaseConnection,
    authority: Authority,
    process: Process,
    lifecycle: Lifecycle,
}

impl Fixture {
    async fn new() -> Result<Self> {
        Self::with_freshness(Duration::from_millis(600)).await
    }

    async fn with_freshness(freshness: Duration) -> Result<Self> {
        let url = std::env::var("BITROUTER_TEST_DATABASE_URL")?;
        let timeout = Duration::from_millis(500);
        let main = store::connect(&url, 1, timeout).await?;
        let heartbeat = store::connect(&url, 1, timeout).await?;
        crate::db::run_migrations(&main).await?;
        let namespace = format!("runtime-{}", uuid::Uuid::new_v4());
        let store = Store::new(main.clone(), &namespace, timeout)?;
        schema::initialize(&store).await?;
        let authority = Authority::new(store, DeploymentKey::from_bytes(rand::random())?);
        authority.initialize().await?;
        authority
            .publish(
                0,
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
        let process = Process {
            server: Server::default(),
            database_url: url,
            shared: serde_json::from_value(serde_json::json!({ "namespace": namespace,
                "deployment_key_env": "UNUSED_RUNTIME_FIXTURE_KEY", "pool_connections": 2,
                "transaction_timeout_ms": 500, "heartbeat_ms": 100, "worker_interval_ms": 100,
                "owner_lease_ms": 10000, "drain_timeout_ms": 2000 }))?,
            telemetry: None,
            operator: None,
        };
        let started = Instant::now();
        let engine = Engine::join(authority.clone(), process.shared.owner_lease_ms).await?;
        let telemetry = Arc::new(Telemetry::new(None)?);
        let generations = Arc::new(Generations::new(
            engine.clone(),
            &process.shared,
            telemetry.clone(),
        )?);
        let health = Health::new(
            generations.serving().clone(),
            freshness,
            started + Duration::from_secs(10),
        );
        let workers = Workers::new(
            engine.heartbeat_handle(heartbeat.clone())?,
            health.clone(),
            &process,
        );
        let lifecycle = Lifecycle::new(engine, generations, workers, health, telemetry);
        Ok(Self {
            main,
            heartbeat,
            authority,
            process,
            lifecycle,
        })
    }

    async fn start(&mut self) -> Result<()> {
        super::super::publication::initial(
            &self.authority,
            &self.process,
            &self.lifecycle.generations,
            &self.lifecycle.health,
        )
        .await?;
        self.lifecycle.workers.start_processing(
            self.authority.clone(),
            self.process.clone(),
            self.lifecycle.generations.clone(),
            self.lifecycle.health.clone(),
            self.lifecycle.engine.clone(),
        )?;
        self.wait_ready(true).await
    }

    async fn wait_ready(&self, ready: bool) -> Result<()> {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let response = self
                    .lifecycle
                    .generations
                    .serving()
                    .router()
                    .oneshot(Request::get("/health").body(Body::empty())?)
                    .await?;
                if (response.status() == StatusCode::OK) == ready {
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .context("runtime readiness did not reach expected state")?
    }

    async fn close(mut self, result: Result<()>) -> Result<()> {
        let deadline = self.lifecycle.shutdown.as_ref().map_or_else(
            || Instant::now() + Duration::from_secs(2),
            |state| state.deadline,
        );
        let force = CancellationToken::new();
        if result.is_err() {
            force.cancel();
        }
        let joined = tokio::time::timeout(
            Duration::from_secs(5),
            self.lifecycle.shutdown_until(deadline, &force),
        )
        .await;
        drop(self.lifecycle);
        let heartbeat = self.heartbeat.close().await;
        let main = self.main.close().await;
        result?;
        ensure!(
            joined
                .context("runtime fixture shutdown stalled")??
                .quiescent
        );
        heartbeat?;
        main?;
        Ok(())
    }
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn retired_source_does_not_stall_current_authority_polling() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let result = async {
        fixture.start().await?;
        let held = fixture.lifecycle.generations.serving().acquire_request()?;
        let head = fixture.authority.head().await?;
        let published = fixture.authority.configuration(&head).await?;
        let next = fixture
            .authority
            .publish(head.config_generation, &published)
            .await?;
        tokio::time::timeout(Duration::from_secs(2), async {
            while fixture.lifecycle.generations.serving().loaded_revision() != Some(next.revision())
            {
                tokio::time::sleep(Duration::from_millis(10)).await;
            }
        })
        .await?;
        fixture.wait_ready(true).await?;
        // More than one freshness lifetime, while the old generation still has
        // a source and its retirement is genuinely unable to complete.
        for _ in 0..12 {
            let response = fixture
                .lifecycle
                .generations
                .serving()
                .router()
                .oneshot(Request::get("/health").body(Body::empty())?)
                .await?;
            ensure!(
                response.status() == StatusCode::OK,
                "old source stalled authority polling"
            );
            tokio::time::sleep(Duration::from_millis(75)).await;
        }
        ensure!(!fixture.lifecycle.generations.retiring().is_empty());
        drop(held);
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn authority_freshness_expires_during_an_observed_heartbeat_lock_wait() -> Result<()> {
    let mut fixture = Fixture::with_freshness(Duration::from_millis(200)).await?;
    let observer = store::connect(&fixture.process.database_url, 1, Duration::from_secs(1)).await?;
    let result = async {
        fixture.start().await?;
        fixture.lifecycle.workers.stop_early();
        fixture.lifecycle.workers.join_early().await;
        let blocker = fixture.authority.store().begin().await?;
        fixture.authority.store().lock::<crate::gateway::shared::authority::Head>(
            &blocker, crate::gateway::shared::store::Kind::Authority, "head").await?;
        let pid: i32 = blocker.query_one(Statement::from_string(DatabaseBackend::Postgres,
            "SELECT pg_backend_pid() AS pid")).await?.context("blocker PID missing")?.try_get("", "pid")?;
        fixture.lifecycle.health.authority(fixture.lifecycle.generations.serving().loaded_revision());
        let blocked = || async {
            // The only live worker touching this held namespace is heartbeat.
            // https://www.postgresql.org/docs/current/functions-info.html#FUNCTIONS-INFO-SESSION-TABLE
            let row = observer.query_one(Statement::from_sql_and_values(DatabaseBackend::Postgres,
                "SELECT EXISTS (SELECT 1 FROM pg_stat_activity WHERE $1 = ANY(pg_blocking_pids(pid))) AS waiting",
                [pid.into()])).await?.context("lock observation missing")?;
            Ok::<bool, anyhow::Error>(row.try_get("", "waiting")?)
        };
        tokio::time::timeout(Duration::from_millis(300), async {
            while !blocked().await? { tokio::time::sleep(Duration::from_millis(5)).await; }
            Ok::<(), anyhow::Error>(())
        }).await??;
        tokio::time::timeout(Duration::from_millis(300), fixture.wait_ready(false)).await??;
        ensure!(blocked().await?, "readiness cleared only after renewal stopped waiting");
        blocker.rollback().await?;
        Ok(())
    }.await;
    let closed = observer.close().await;
    let result = result.and(closed.map_err(anyhow::Error::from));
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn maintenance_stop_does_not_wait_for_an_exhausted_main_pool() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let result = async {
        fixture.start().await?;
        let occupied = fixture.authority.store().begin().await?;
        let digest = "a".repeat(64);
        // Poll admission before cancelling: these are real UnknownAck entries,
        // whose reconciliation must inspect SQL, rather than NotStarted slots.
        for _ in 0..64 {
            let (snapshot, reservation, source) =
                fixture.lifecycle.generations.serving().acquire_request()?;
            let request = crate::gateway::shared::engine::Admission {
                key_hash: &digest,
                correlation_id: "maintenance-fixture",
                idempotency_key: None,
                fingerprint: &digest,
                operation: bitrouter_ai::types::ModelOperation::Generation,
                revision: snapshot.revision,
            };
            let mut admission = Box::pin(reservation.admit(&request));
            ensure!(futures::poll!(&mut admission).is_pending());
            drop(admission);
            drop((source, snapshot));
        }
        tokio::time::timeout(Duration::from_millis(300), async {
            while fixture
                .lifecycle
                .generations
                .serving()
                .custody
                .reconciling()
                == 0
            {
                tokio::time::sleep(Duration::from_millis(5)).await;
            }
        })
        .await
        .context("custody never entered its blocked database operation")?;
        fixture.lifecycle.workers.stop_early();
        tokio::time::timeout(
            Duration::from_millis(100),
            fixture.lifecycle.workers.join_early(),
        )
        .await?;
        ensure!(
            fixture
                .lifecycle
                .workers
                .reports()
                .iter()
                .all(|(name, report)| *name == "heartbeat" || report.joined)
        );
        // No cancellation discarded the custodial queue or its permits.
        ensure!(fixture.lifecycle.generations.serving().custody.pending() == 64);
        ensure!(
            fixture
                .lifecycle
                .generations
                .serving()
                .custody
                .reconciling()
                == 0
        );
        occupied.rollback().await?;
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn main_pool_loss_clears_readiness_while_original_heartbeat_continues() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let result = async {
        fixture.start().await?;
        let lease = fixture
            .lifecycle
            .health
            .lease_deadline()
            .context("lease missing")?;
        let occupied = fixture.authority.store().begin().await?;
        // Only the occupancy fixture stays idle beyond the production deadline.
        // https://www.postgresql.org/docs/current/runtime-config-client.html
        occupied
            .execute(Statement::from_string(
                DatabaseBackend::Postgres,
                "SET LOCAL idle_in_transaction_session_timeout = '5s'",
            ))
            .await?;
        fixture.wait_ready(false).await?;
        ensure!(
            fixture
                .lifecycle
                .health
                .lease_deadline()
                .is_some_and(|current| current > lease)
        );
        occupied.rollback().await?;
        fixture.wait_ready(true).await?;
        let deadline = Instant::now() + Duration::from_secs(2);
        let report = fixture
            .lifecycle
            .shutdown_until(deadline, &CancellationToken::new())
            .await?;
        ensure!(
            report.clean && report.quiescent && report.durable_resolved && !report.forced,
            "{report:?}"
        );
        fixture.wait_ready(false).await?;
        fixture.lifecycle.health.refresh();
        fixture.wait_ready(false).await?;
        // Re-observing a completed drain cannot turn it into forced shutdown.
        let repeated = fixture
            .lifecycle
            .shutdown_until(deadline, &CancellationToken::new())
            .await?;
        ensure!(repeated.clean && !repeated.forced);
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelled_withdrawal_resumes_the_original_deadline_without_sql_cleanup() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let result = async {
        fixture.start().await?;
        let occupied = fixture.authority.store().begin().await?;
        let deadline = Instant::now() + Duration::from_millis(100);
        let force = CancellationToken::new();
        ensure!(
            tokio::time::timeout(
                Duration::from_millis(25),
                fixture.lifecycle.shutdown_until(deadline, &force)
            )
            .await
            .is_err()
        );
        ensure!(fixture.lifecycle.report().withdrawal == Withdrawal::Unknown);
        ensure!(
            fixture
                .lifecycle
                .shutdown_until(deadline + Duration::from_secs(1), &force)
                .await
                .is_err()
        );
        fixture.wait_ready(false).await?;
        tokio::time::sleep_until(deadline).await;
        // The main connection is still occupied. A forced join must not try to
        // close a producer or perform compensation with a fresh SQL deadline.
        let report = tokio::time::timeout(
            Duration::from_secs(1),
            fixture.lifecycle.shutdown_until(deadline, &force),
        )
        .await??;
        ensure!(
            report.forced && report.quiescent && !report.clean && !report.durable_resolved,
            "{report:?}"
        );
        ensure!(report.withdrawal == Withdrawal::Unknown);
        occupied.rollback().await?;
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn transport_children_release_generation_sources_before_pipeline_force() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let result = async {
        fixture.start().await?;
        let entered = Arc::new(Notify::new());
        let serving = fixture.lifecycle.generations.serving().clone();
        let handler_entered = entered.clone();
        let router = Router::new().route(
            "/held",
            get(move || {
                let serving = serving.clone();
                let entered = handler_entered.clone();
                async move {
                    if let Ok(owners) = serving.acquire_request() {
                        entered.notify_one();
                        std::future::pending::<()>().await;
                        drop(owners);
                    }
                    StatusCode::SERVICE_UNAVAILABLE
                }
            }),
        );
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        fixture.lifecycle.transport = Some(Transport::start(
            listener,
            router,
            (&fixture.process.shared).into(),
        )?);
        let request =
            tokio::spawn(async move { reqwest::get(format!("http://{address}/held")).await });
        let checked = async {
            tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
            let deadline = Instant::now() + Duration::from_millis(100);
            let report = tokio::time::timeout(
                Duration::from_secs(2),
                fixture
                    .lifecycle
                    .shutdown_until(deadline, &CancellationToken::new()),
            )
            .await??;
            ensure!(
                report.forced && report.quiescent && !report.durable_resolved,
                "{report:?}"
            );
            ensure!(
                fixture
                    .lifecycle
                    .transport
                    .as_ref()
                    .is_some_and(|transport| transport.report().connections.cancelled == 1)
            );
            ensure!(
                fixture
                    .lifecycle
                    .generations
                    .forced_reports()
                    .iter()
                    .all(|report| report.is_quiescent() && !report.pipeline_failed)
            );
            Ok(())
        }
        .await;
        request.abort();
        if let Err(error) = request.await {
            ensure!(error.is_cancelled(), "fixture client panicked");
        }
        checked
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn startup_without_transport_still_joins_heartbeat_and_telemetry() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let result = async {
        // This is the same retained-owner path used for early build/listen errors.
        let report = fixture
            .lifecycle
            .shutdown_until(
                Instant::now() + Duration::from_secs(2),
                &CancellationToken::new(),
            )
            .await?;
        ensure!(
            report.clean && report.quiescent && report.durable_resolved,
            "{report:?}"
        );
        ensure!(fixture.lifecycle.workers.reports().len() == 1);
        Ok(())
    }
    .await;
    fixture.close(result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelled_dual_listener_drain_resumes_and_joins_both_before_pipeline_force() -> Result<()>
{
    use tokio::io::{AsyncReadExt as _, AsyncWriteExt as _};

    let mut fixture = Fixture::new().await?;
    let result = async {
        fixture.start().await?;
        let mut sockets = Vec::new();
        for inference in [true, false] {
            let entered = Arc::new(Notify::new());
            let handler_entered = entered.clone();
            let serving = fixture.lifecycle.generations.serving().clone();
            let router = Router::new().route(
                "/held",
                get(move || {
                    let serving = serving.clone();
                    let entered = handler_entered.clone();
                    async move {
                        let owners = inference.then(|| serving.acquire_request()).transpose();
                        if owners.is_ok() {
                            entered.notify_one();
                            std::future::pending::<()>().await;
                        }
                        drop(owners);
                        StatusCode::SERVICE_UNAVAILABLE
                    }
                }),
            );
            let listener = TcpListener::bind("127.0.0.1:0").await?;
            let address = listener.local_addr()?;
            let transport = Transport::start(listener, router, (&fixture.process.shared).into())?;
            if inference {
                fixture.lifecycle.transport = Some(transport);
            } else {
                fixture.lifecycle.operator_transport = Some(transport);
            }
            let mut socket = tokio::net::TcpStream::connect(address).await?;
            socket
                .write_all(b"GET /held HTTP/1.1\r\nHost: localhost\r\n\r\n")
                .await?;
            tokio::time::timeout(Duration::from_secs(2), entered.notified()).await?;
            sockets.push(socket);
        }
        let deadline = Instant::now() + Duration::from_millis(200);
        let force = CancellationToken::new();
        ensure!(
            tokio::time::timeout(
                Duration::from_millis(25),
                fixture.lifecycle.shutdown_until(deadline, &force)
            )
            .await
            .is_err()
        );
        for transport in fixture
            .lifecycle
            .transport
            .iter()
            .chain(fixture.lifecycle.operator_transport.iter())
        {
            ensure!(transport.report().connections.pending == 1);
        }
        ensure!(
            fixture
                .lifecycle
                .shutdown_until(deadline + Duration::from_secs(1), &force)
                .await
                .is_err()
        );
        let report = tokio::time::timeout(
            Duration::from_secs(2),
            fixture.lifecycle.shutdown_until(deadline, &force),
        )
        .await??;
        ensure!(report.forced && report.quiescent && !report.clean);
        for transport in fixture
            .lifecycle
            .transport
            .iter()
            .chain(fixture.lifecycle.operator_transport.iter())
        {
            let report = transport.report();
            ensure!(
                report.is_quiescent()
                    && report.connections.cancelled == 1
                    && report.connections.joined == 1
            );
        }
        ensure!(
            fixture
                .lifecycle
                .generations
                .forced_reports()
                .iter()
                .all(|report| report.is_quiescent() && !report.pipeline_failed)
        );
        for mut socket in sockets {
            let mut bytes = Vec::new();
            tokio::time::timeout(Duration::from_secs(1), socket.read_to_end(&mut bytes)).await??;
        }
        Ok(())
    }
    .await;
    fixture.close(result).await
}
