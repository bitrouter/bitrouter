//! Opt-in local resource authority. All authorization and storage stay in the host.

mod actor;
mod admission;
pub(crate) mod audit;
mod authority;
mod cache;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod capture;
mod command;
mod configuration;
mod configuration_store;
pub(crate) mod continuation;
mod directory;
mod error;
#[cfg(feature = "core")]
pub(crate) mod eval;
pub(crate) mod execution;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod experiment;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod hosts;
pub(crate) mod http;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod learner;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod learning;
mod model;
mod mutation;
pub(crate) mod oauth;
pub(crate) mod operational;
mod ordered;
pub(crate) mod origin;
pub(crate) mod ownership;
mod profile_observation;
pub(crate) mod receipts;
pub(crate) mod resources;
pub(crate) mod routing;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod session;
pub(crate) mod storage;
mod store;
#[cfg(test)]
mod tests;
#[cfg(feature = "core")]
pub(crate) mod trajectory;
pub(super) mod trajectory_profile;
mod transaction;

use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::Duration;

use bitrouter_ai::tenancy::{
    authority::{CredentialState, CredentialStatus, Permission},
    identity::{CredentialId, InstallationId, InstanceId, OperationId, PrincipalId, TenantId},
    operation::{ErrorCode, OperationResult, TenantCapabilities},
};
use chrono::Utc;
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use sea_orm::DatabaseConnection;
use tokio::sync::{Semaphore, SemaphorePermit, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use self::{
    actor::{CredentialRead, Job, Liveness, Message, Worker},
    command::Command,
    error::Failure,
    model::{Caller, Snapshot},
    store::StoredResponse,
};

/// Current authority store format, shared by live and offline target readers.
/// Historical migrations keep their own fixed version transitions.
pub(crate) const AUTHORITY_SCHEMA_VERSION: i32 = 5;

const MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(5);

struct Provisioning<'a> {
    import: Option<Vec<u8>>,
    continuations: &'a crate::config::ContinuationConfig,
    claim: Option<ownership::Claim>,
    receipt_observer: crate::telemetry::receipt_latency::Sink,
    config: &'a crate::config::tenancy::TenancyConfig,
}

// The startup test installs H's existing fault gate before the actor can run.
// No global hook or alternate transaction implementation is involved.
#[cfg(test)]
struct BootstrapGate {
    phase: transaction::Phase,
    attached: oneshot::Sender<(Arc<transaction::Proof>, Arc<transaction::Gate>)>,
}

// Per-startup scheduling only, with the real connector/Worker/Admission. No
// global hook, replacement snapshot, or alternate transaction implementation.
#[cfg(test)]
struct StartupTest {
    bootstrap: Option<BootstrapGate>,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    bind: Option<CapacityBindTest>,
}
#[cfg(all(test, any(feature = "otel-http", feature = "otel-grpc")))]
type CapacityBindTest =
    Box<dyn FnOnce(&Service, &dyn Fn() -> anyhow::Result<()>) -> anyhow::Result<()> + Send>;

#[derive(Clone)]
pub(crate) struct Service {
    #[cfg(test)]
    technical_io: Arc<actor::provider_technical::tests::State>,
    startup: Option<Arc<origin::StartupOwner>>,
    db: DatabaseConnection,
    authority: Arc<authority::live::Authority>,
    live: Arc<AtomicBool>,
    instance: InstanceId,
    commands: mpsc::Sender<Job>,
    safety: mpsc::Sender<Job>,
    completions: mpsc::Sender<Job>,
    receipt_volume: Option<std::path::PathBuf>,
    receipt_observer: crate::telemetry::receipt_latency::Sink,
    stop: CancellationToken,
    accepting: Arc<std::sync::Mutex<bool>>,
    cleanup: Arc<transaction::Proof>,
    #[cfg(all(feature = "core", feature = "harness"))]
    hosts: Arc<hosts::Hosts>,
    #[cfg(all(feature = "core", feature = "harness"))]
    learning_worker: Arc<AtomicBool>,
    #[cfg(all(test, unix, feature = "core", feature = "harness"))]
    learner_test: Arc<learner::testing::Hooks>,
    #[cfg(feature = "core")]
    eval: Arc<eval::State>,
    reads: Arc<Semaphore>,
    resources: Arc<resources::Resources>,
    native_continuation: Option<continuation::PinnedCrypto>,
    http_continuation: Arc<continuation::http::Port>,
    http_registry: crate::continuation::ContinuationRegistry,
    caches: Arc<execution::cached::Caches>,
    admission: Arc<admission::Admission>,
    #[cfg(all(test, any(feature = "otel-http", feature = "otel-grpc")))]
    obligation_due: Arc<std::sync::Mutex<Option<chrono::DateTime<chrono::Utc>>>>,
    executor: Arc<crate::gateway::pipeline::executor::HttpExecutor>,
    execution_settings: crate::config::tenancy::TenancyConfig,
    private_adapter: Option<Arc<execution::private_adapter::Peer>>,
    routing: Arc<routing::state::Runtime>,
    operational: Arc<operational::state::Runtime>,
    #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
    operational_exporter: Arc<std::sync::OnceLock<Arc<crate::telemetry::exporter::OtelExporter>>>,
    pipelines: Arc<execution::lifecycle::Registry>,
    #[cfg(test)]
    dispatch_test_ready: Arc<AtomicBool>,
    #[cfg(test)]
    receipt_credential_gate:
        Arc<std::sync::Mutex<Option<Arc<receipts::authority_test::CredentialGate>>>>,
    #[cfg(all(test, unix, feature = "core", feature = "harness"))]
    choice_final_gate: Arc<std::sync::Mutex<Option<Arc<receipts::authority_test::CredentialGate>>>>,
    #[cfg(all(test, unix, feature = "core", feature = "harness"))]
    paid_finish_gate: Arc<std::sync::Mutex<Option<Arc<receipts::authority_test::CredentialGate>>>>,
    completion: Shared<BoxFuture<'static, Result<(), Arc<tokio::task::JoinError>>>>,
}

impl Service {
    pub(crate) fn storage_budget(&self) -> storage::Budget {
        storage::Budget::new(self.admission.clone(), self.cleanup.clone())
    }

    pub(crate) fn execution_settings(&self) -> &crate::config::tenancy::TenancyConfig {
        &self.execution_settings
    }

    pub(crate) async fn open_owned_profile(
        database: crate::db::OwnedDatabase,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
    ) -> anyhow::Result<Self> {
        Self::open_owned_profile_observed(
            database,
            config,
            continuations,
            crate::telemetry::receipt_latency::Sink::default(),
        )
        .await
    }

    pub(crate) async fn open_owned_profile_observed(
        database: crate::db::OwnedDatabase,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
        observer: crate::telemetry::receipt_latency::Sink,
    ) -> anyhow::Result<Self> {
        Self::open_owned_profile_observed_startup(
            database,
            config,
            continuations,
            observer,
            #[cfg(test)]
            None,
        )
        .await
    }

    async fn open_owned_profile_observed_startup(
        database: crate::db::OwnedDatabase,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
        observer: crate::telemetry::receipt_latency::Sink,
        #[cfg(test)] startup_test: Option<StartupTest>,
    ) -> anyhow::Result<Self> {
        let (db, owner, claim) = database.claim()?;
        Self::open_configured_origin(
            db,
            Some(owner),
            config,
            continuations,
            (Some(claim), observer),
            #[cfg(test)]
            startup_test,
        )
        .await
    }

    #[cfg(test)]
    pub(crate) async fn open(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
    ) -> anyhow::Result<Self> {
        Self::open_configured(
            db,
            owner,
            &crate::config::tenancy::TenancyConfig::default(),
            &crate::config::ContinuationConfig::default(),
            crate::telemetry::receipt_latency::Sink::default(),
        )
        .await
    }

    #[cfg(all(test, unix))]
    pub(crate) async fn open_profile(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
    ) -> anyhow::Result<Self> {
        Self::open_configured(
            db,
            owner,
            config,
            continuations,
            crate::telemetry::receipt_latency::Sink::default(),
        )
        .await
    }

    pub(crate) fn receipt_observer(&self) -> crate::telemetry::receipt_latency::Sink {
        self.receipt_observer.clone()
    }

    #[cfg(test)]
    async fn open_configured(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
        receipt_observer: crate::telemetry::receipt_latency::Sink,
    ) -> anyhow::Result<Self> {
        Self::open_configured_origin(
            db,
            owner,
            config,
            continuations,
            (None, receipt_observer),
            None,
        )
        .await
    }

    async fn open_configured_origin(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
        origin: (
            Option<ownership::Claim>,
            crate::telemetry::receipt_latency::Sink,
        ),
        #[cfg(test)] startup_test: Option<StartupTest>,
    ) -> anyhow::Result<Self> {
        let (claim, receipt_observer) = origin;
        config.validate_lifetime()?;
        config.capacity.validate()?;
        let initial = store::initial_header_with(&db, Utc::now(), || {
            if let Some(owner) = &owner {
                return storage::exclusive_write(owner, 16 * 1024);
            }
            // In-memory test services have no file volume or exclusive lease.
            anyhow::ensure!(cfg!(test), "startup database owner missing");
            Ok(())
        })
        .await?;
        let resources = Arc::new(resources::Resources::load_key(
            &initial.installation,
            config,
        )?);
        let import = resources::Resources::read_import(config.resource_import.as_deref()).await?;
        let service = Self::start_resources_with_continuations(
            db,
            owner,
            initial,
            resources,
            Provisioning {
                import,
                continuations,
                claim,
                receipt_observer,
                config,
            },
            #[cfg(test)]
            startup_test,
        )
        .await?;
        #[cfg(all(feature = "core", feature = "harness"))]
        if let Err(error) =
            learner::registration::initialize(&service, config.learning_registrations.as_deref())
                .await
        {
            // H bootstrap and crypto succeeded. Failure must drain this actual
            // Service before returning, before any listener or worker exposure.
            let stopped = service.shutdown().await;
            stopped?;
            return Err(error);
        }
        if service.http_continuation.state.enabled() {
            service.http_registry.start_reconciler();
        }
        Ok(service)
    }

    #[cfg(test)]
    async fn start_prepared(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        initial: Snapshot,
        resources: Arc<resources::Resources>,
    ) -> anyhow::Result<Self> {
        Self::start_resources(db, owner, initial, resources, None, None).await
    }

    #[cfg(test)]
    async fn start_resources(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        initial: Snapshot,
        resources: Arc<resources::Resources>,
        import: Option<Vec<u8>>,
        #[cfg(test)] bootstrap_gate: Option<BootstrapGate>,
    ) -> anyhow::Result<Self> {
        // Transaction fault fixtures control the sole writer's next operation.
        // Background timers are started by open_configured, or explicitly by
        // the tests that exercise reconciliation, after their gates are ready.
        Self::start_resources_with_continuations(
            db,
            owner,
            initial,
            resources,
            Provisioning {
                import,
                continuations: &crate::config::ContinuationConfig::default(),
                claim: None,
                receipt_observer: crate::telemetry::receipt_latency::Sink::default(),
                config: &crate::config::tenancy::TenancyConfig::default(),
            },
            bootstrap_gate.map(|bootstrap| StartupTest {
                bootstrap: Some(bootstrap),
                #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
                bind: None,
            }),
        )
        .await
    }

    async fn start_resources_with_continuations(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        initial: Snapshot,
        resources: Arc<resources::Resources>,
        provisioning: Provisioning<'_>,
        #[cfg(test)] mut startup_test: Option<StartupTest>,
    ) -> anyhow::Result<Self> {
        let Provisioning {
            import,
            continuations,
            claim,
            receipt_observer,
            config,
        } = provisioning;
        config.capacity.validate()?;
        let capacity = &config.capacity;
        tracing::info!(?capacity, "tenant runtime budgets selected");
        let read_timeout = Duration::from_secs(config.stream_idle_timeout_secs);
        let private_adapter = config
            .private_adapter
            .as_ref()
            .map(|adapter| execution::private_adapter::Peer::load(adapter, read_timeout))
            .transpose()?
            .map(Arc::new);
        // Construct fallible transport state before spawning the owning actor.
        // Tenant profiles share connection pools, never host credential plugins.
        let executor = Arc::new(crate::gateway::pipeline::executor::HttpExecutor::new(
            crate::gateway::model::client::HttpTimeouts {
                read: read_timeout,
                ..Default::default()
            },
        )?);
        let caches = Arc::new(execution::cached::Caches::new(&capacity.caches)?);
        let routing = Arc::new(routing::state::Runtime::configured(
            &capacity.routing,
            capacity.execution.instance_active,
        )?);
        let live = Arc::new(AtomicBool::new(true));
        let authority = authority::live::Authority::new(
            initial.header(),
            &capacity.authority_views,
            live.clone(),
        )
        .map_err(|code| anyhow::anyhow!("authority view budget unavailable: {code:?}"))?;
        let cleanup = transaction::Proof::new(live.clone());
        #[cfg(test)]
        if let Some(bootstrap_gate) = startup_test.as_mut().and_then(|test| test.bootstrap.take()) {
            let gate = cleanup.hold(bootstrap_gate.phase);
            bootstrap_gate
                .attached
                .send((cleanup.clone(), gate))
                .map_err(|_| anyhow::anyhow!("bootstrap test gate receiver unavailable"))?;
        }
        let instance: InstanceId = uuid::Uuid::new_v4().to_string().try_into()?;
        let startup =
            claim.map(|claim| origin::StartupOwner::new(claim, instance.clone(), cleanup.clone()));
        // Ordinary traffic cannot occupy the queue reserved for installation
        // revokes and stop transitions. Neither queue waits for free capacity.
        let (commands, receiver) = mpsc::channel(capacity.management.queue);
        let (safety, safety_receiver) = mpsc::channel(capacity.management.safety_queue);
        let (completions, completion_receiver) = mpsc::channel(capacity.execution.instance_active);
        let receipt_volume = owner
            .as_ref()
            .and_then(|owner| owner.path.parent().map(std::path::Path::to_path_buf));
        let admission = admission::Admission::configured(capacity);
        admission
            .bind_receipt_volume(receipt_volume.clone())
            .map_err(|_| anyhow::anyhow!("receipt volume observation failed"))?;
        let accepting = Arc::new(std::sync::Mutex::new(true));
        #[cfg(feature = "core")]
        let eval = eval::State::new(live.clone());
        let http_state = crate::continuation::tenant::state::State::new(
            continuations,
            instance.as_str().to_owned(),
        )?;
        let reads = Arc::new(Semaphore::new(capacity.management.reads));
        let http_continuation = continuation::http::Port::new(
            commands.clone(),
            &cleanup,
            reads.clone(),
            http_state.clone(),
        );
        let http_registry =
            crate::continuation::ContinuationRegistry::for_tenant(http_continuation.clone())?;
        let obligation_due = Arc::new(std::sync::Mutex::new(None));
        #[cfg(test)]
        let technical_io = Arc::new(actor::provider_technical::tests::State::default());
        let worker = Worker {
            #[cfg(test)]
            technical_io: technical_io.clone(),
            obligation_due: obligation_due.clone(),
            startup: startup.clone(),
            db: db.clone(),
            authority: authority.clone(),
            live: Liveness(live.clone()),
            owner,
            resources: resources.clone(),
            proof: cleanup.clone(),
            admission: admission.clone(),
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            observe_capacity: receipt_observer.enabled(),
            http_continuation: http_state,
            #[cfg(feature = "core")]
            eval: eval.clone(),
            #[cfg(feature = "core")]
            accepting: accepting.clone(),
        };
        let stop = CancellationToken::new();
        let (initialized, initialization) = oneshot::channel();
        let worker = tokio::spawn(worker.run(
            receiver,
            safety_receiver,
            completion_receiver,
            stop.clone(),
            import,
            initialized,
        ));
        let completion = async move { worker.await.map_err(Arc::new) }
            .boxed()
            .shared();
        let mut service = Self {
            #[cfg(test)]
            technical_io,
            #[cfg(all(test, any(feature = "otel-http", feature = "otel-grpc")))]
            obligation_due,
            startup,
            db,
            authority,
            live,
            instance,
            commands,
            safety,
            completions,
            receipt_volume,
            receipt_observer,
            stop,
            accepting,
            #[cfg(feature = "core")]
            eval,
            cleanup,
            #[cfg(all(feature = "core", feature = "harness"))]
            hosts: Arc::new(hosts::Hosts::new()),
            #[cfg(all(feature = "core", feature = "harness"))]
            learning_worker: Arc::new(AtomicBool::new(false)),
            #[cfg(all(test, unix, feature = "core", feature = "harness"))]
            learner_test: Arc::default(),
            reads,
            resources,
            native_continuation: None,
            http_continuation,
            http_registry,
            caches,
            admission,
            executor,
            execution_settings: config.clone(),
            private_adapter,
            routing,
            operational: Arc::default(),
            #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
            operational_exporter: Arc::default(),
            pipelines: execution::lifecycle::Registry::with_capacity(
                capacity.execution.instance_active,
            ),
            #[cfg(test)]
            dispatch_test_ready: Arc::new(AtomicBool::new(false)),
            #[cfg(all(test, unix, feature = "core", feature = "harness"))]
            choice_final_gate: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(all(test, unix, feature = "core", feature = "harness"))]
            paid_finish_gate: Arc::new(std::sync::Mutex::new(None)),
            #[cfg(test)]
            receipt_credential_gate: Arc::default(),
            completion,
        };
        match initialization.await {
            Ok(Ok(initialized)) => {
                service.native_continuation = initialized.crypto;
                // Bind the Worker's post-bootstrap copy; maintenance may already
                // hold Admission here. SDK callbacks still use pure try_lock.
                let bind = || {
                    service.bind_receipt_capacity(
                        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
                        initialized.capacity,
                    )
                };
                #[cfg(all(test, any(feature = "otel-http", feature = "otel-grpc")))]
                let bound = match startup_test.and_then(|test| test.bind) {
                    Some(hook) => hook(&service, &bind),
                    None => bind(),
                };
                #[cfg(not(all(test, any(feature = "otel-http", feature = "otel-grpc"))))]
                let bound = bind();
                if let Err(error) = bound {
                    service.shutdown().await?;
                    return Err(error);
                }
                Ok(service)
            }
            _ => {
                service.shutdown().await?;
                anyhow::bail!("native continuation key initialization failed")
            }
        }
    }

    fn bind_receipt_capacity(
        &self,
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))] initial: Option<
            crate::telemetry::receipt_latency::capacity::Snapshot,
        >,
    ) -> anyhow::Result<()> {
        #[cfg(any(feature = "otel-http", feature = "otel-grpc"))]
        if self.receipt_observer.enabled() {
            use crate::telemetry::receipt_latency::capacity::ReadError;
            let initial = initial
                .ok_or_else(|| anyhow::anyhow!("receipt capacity bootstrap snapshot missing"))?;
            let source = Arc::downgrade(&self.admission);
            anyhow::ensure!(
                self.receipt_observer.bind_capacity(initial, move || {
                    source
                        .upgrade()
                        .ok_or(ReadError::Unavailable)?
                        .observed_capacity()
                }),
                "receipt capacity observation already bound or sealed"
            );
            let caches = Arc::downgrade(&self.caches);
            let routing = Arc::downgrade(&self.routing);
            anyhow::ensure!(
                self.receipt_observer.bind_runtime_capacity(move || {
                    let [executions, credentials] = caches
                        .upgrade()
                        .map(|caches| caches.observed())
                        .unwrap_or([Err(ReadError::Unavailable); 2]);
                    let routing = routing
                        .upgrade()
                        .ok_or(ReadError::Unavailable)
                        .and_then(|routing| routing.observed());
                    crate::telemetry::receipt_latency::capacity::runtime::Snapshot {
                        executions,
                        credentials,
                        routing,
                    }
                }),
                "runtime capacity observation already bound"
            );
        }
        Ok(())
    }

    pub(crate) fn continuation_registry(&self) -> &crate::continuation::ContinuationRegistry {
        &self.http_registry
    }

    /// Fence new dispatch before acknowledging process stop. This does not wait
    /// for SQL, disk probes, receipt accounting or a producer to finish.
    pub(crate) fn stop_dispatch(&self) -> anyhow::Result<()> {
        *self
            .accepting
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant ingress lock unavailable"))? = false;
        self.authority
            .with_write(|| {
                self.caches.close();
                Ok(())
            })
            .map_err(|_| anyhow::anyhow!("tenant publication gate unavailable at shutdown"))?;
        Ok(())
    }

    /// Stop accepting mutations and drain both admitted command queues. Timeout
    /// never aborts the writer or releases its file lease; the host must report
    /// unsuccessful shutdown instead of claiming all transactions settled.
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.stop_dispatch()?;
        self.admission.close();
        self.pipelines.close();
        #[cfg(all(feature = "core", feature = "harness"))]
        self.hosts.close();
        let draining = async {
            #[cfg(all(feature = "core", feature = "harness"))]
            tokio::time::timeout(Duration::from_secs(30), self.hosts.drain())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("native host cleanup did not drain; owner retained")
                })??;
            #[cfg(all(feature = "core", feature = "harness"))]
            if self.startup.is_some() {
                // Optional recovery evidence never substitutes for ordinary
                // shutdown. Unsupported coverage leaves no stamp and the
                // unchanged full drain below still decides cleanup success.
                let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                if let Ok(Some(proof)) = self.pipelines.paid_stop_set(deadline).await {
                    crate::tenancy::learning::queue::stop_set(proof, deadline)
                        .await
                        .map_err(|_| anyhow::anyhow!("paid stop-set retention unconfirmed"))?;
                }
            }
            tokio::time::timeout(Duration::from_secs(30), self.pipelines.close_and_drain())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("tenant pipeline cleanup did not drain within 30 seconds")
                })??;
            tokio::time::timeout(Duration::from_secs(30), self.admission.wait_drained())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("tenant executions did not drain within 30 seconds")
                })?;
            if self.http_continuation.state.enabled() {
                let deadline = tokio::time::Instant::now() + Duration::from_secs(30);
                self.http_continuation
                    .close_producer(deadline)
                    .await
                    .map_err(|_| {
                        anyhow::anyhow!("HTTP continuation producer barrier unavailable")
                    })?;
                let drained = self.http_registry.drain_owned_until(deadline).await?;
                anyhow::ensure!(
                    drained.is_clean(),
                    "HTTP continuation cleanup retained ownership"
                );
            }
            #[cfg(feature = "core")]
            {
                let _gate = self
                    .accepting
                    .lock()
                    .map_err(|_| anyhow::anyhow!("tenant ingress lock unavailable"))?;
                anyhow::ensure!(
                    self.eval.healthy(),
                    "required tenant observation failed; owner retained"
                );
                self.eval.producers_open.store(false, Ordering::Release);
            }
            self.stop.cancel();
            tokio::time::timeout(Duration::from_secs(30), self.completion.clone())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("tenant authority shutdown did not settle within 30 seconds")
                })?
                .map_err(|_| anyhow::anyhow!("tenant authority worker ended unexpectedly"))?;
            self.cleanup.ensure_clean()
        };
        tokio::select! {
            biased;
            () = self.cleanup.quarantined() => Err(anyhow::anyhow!("tenant transaction cleanup is unconfirmed; owner retained")),
            result = draining => {
                result?;
                self.cleanup.ensure_clean()
            }
        }
    }

    fn current(&self) -> Result<Arc<Snapshot>, Failure> {
        if !self.live.load(Ordering::Acquire) {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        self.authority
            .read()
            .map(|guard| Arc::new(guard.snapshot()))
            .map_err(Failure::from)
    }

    fn unavailable(&self) -> Failure {
        self.live.store(false, Ordering::Release);
        ErrorCode::StorageUnavailable.into()
    }

    fn read_permit(&self) -> Result<SemaphorePermit<'_>, Failure> {
        // Do not create an unbounded queue of multi-megabyte retained reads on
        // the single SQLite pool. Ordinary read pressure is not storage failure.
        self.reads
            .try_acquire()
            .map_err(|_| ErrorCode::CapacityExceeded.into())
    }

    #[cfg(test)]
    async fn caller_facts_for_test(&self, caller: &Caller) -> Result<Snapshot, Failure> {
        let identity = self
            .authority_read(actor::AuthoritySubject::Caller(caller.clone()))
            .await?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| Ok(view.facts().clone()))
            .map_err(Failure::from)
    }

    #[cfg(test)]
    async fn authenticate(&self, token: &str) -> Result<Caller, Failure> {
        self.authenticate_live(token)
            .await
            .map(|identity| identity.caller)
    }

    async fn authenticate_live(
        &self,
        token: &str,
    ) -> Result<authority::live::Authenticated, Failure> {
        self.authenticate_with_reuse(token, false).await
    }

    async fn authenticate_with_reuse(
        &self,
        token: &str,
        reuse: bool,
    ) -> Result<authority::live::Authenticated, Failure> {
        if !token.starts_with("brtk_") || token.len() > 512 {
            return Err(ErrorCode::Unauthorized.into());
        }
        self.authority_read(actor::AuthoritySubject::Token {
            hash: crate::auth::keys::hash_key(token),
            reuse,
        })
        .await
    }

    async fn authority_read(
        &self,
        subject: actor::AuthoritySubject,
    ) -> Result<authority::live::Authenticated, Failure> {
        self.admitted_authority_read(|| Ok(subject)).await
    }

    async fn admitted_authority_read(
        &self,
        prepare: impl FnOnce() -> Result<actor::AuthoritySubject, Failure>,
    ) -> Result<authority::live::Authenticated, Failure> {
        let permit = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let subject = prepare()?;
        let deadline = tokio::time::Instant::now() + MANAGEMENT_TIMEOUT;
        let (reply, receive) = oneshot::channel();
        self.enqueue(
            Job::AuthorityRead(actor::AuthorityRead {
                subject,
                deadline,
                permit,
                reply,
            }),
            false,
        )?;
        tokio::time::timeout_at(deadline, receive)
            .await
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
    }

    fn check_installation(&self, expected: &InstallationId) -> Result<(), Failure> {
        if &self.current()?.installation == expected {
            Ok(())
        } else {
            Err(ErrorCode::CapabilityMismatch.into())
        }
    }

    async fn capabilities(&self, caller: &Caller) -> Result<TenantCapabilities, Failure> {
        self.with_capability_authority(caller, |state| self.capability_document(state))
            .await
    }

    async fn with_capability_authority<T>(
        &self,
        caller: &Caller,
        document: impl FnOnce(&Snapshot) -> Result<T, ErrorCode>,
    ) -> Result<T, Failure> {
        let identity = self
            .authority_read(actor::AuthoritySubject::Caller(caller.clone()))
            .await?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                view.facts().principal(caller, Utc::now())?;
                document(view.facts())
            })
            .map_err(Failure::from)
    }

    fn capability_document(&self, state: &Snapshot) -> Result<TenantCapabilities, ErrorCode> {
        // All three guarantees derive from the same current eligibility fences;
        // discovery does not grant authority over any selected retained route.
        let dispatch_ready = self.dispatch_ready(state);
        Ok(TenantCapabilities {
            contract_version: 1,
            installation_id: state.installation.clone(),
            instance_id: self.instance.clone(),
            capability_revision: self.capability_revision(state)?,
            revision_bound_dispatch: dispatch_ready,
            strict_receipts: dispatch_ready,
            dispatch_enabled: dispatch_ready,
        })
    }

    async fn execute(&self, caller: Caller, command: Command) -> Result<StoredResponse, Failure> {
        if self.stop.is_cancelled() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        command
            .validate()
            .map_err(|_| Failure::from(ErrorCode::InvalidRequest))?;
        self.check_installation(command.installation())?;
        let operation = command.operation_id().clone();
        let safety = matches!(&caller, Caller::Installation(_)) && command.safety();
        let (reply, receive) = oneshot::channel();
        let message = Message {
            caller,
            command,
            native_adapter_configured: self.private_adapter.is_some(),
            reply,
        };
        self.enqueue(message.into(), safety)?;
        match tokio::time::timeout(MANAGEMENT_TIMEOUT, receive).await {
            Ok(Ok(response)) => response,
            _ => Err(Failure::new(
                ErrorCode::AuthorityUnavailable,
                Some(operation),
            )),
        }
    }

    fn enqueue(&self, job: Job, safety: bool) -> Result<(), Failure> {
        let operation = match &job {
            #[cfg(all(feature = "core", feature = "harness"))]
            Job::Native(_)
            | Job::Capture(_)
            | Job::Learning(_)
            | Job::Experiment(_)
            | Job::ExperimentRead(_)
            | Job::Learner(_) => None,
            Job::Command(message) => Some(message.command.operation_id().clone()),
            Job::OAuth(_)
            | Job::Directory(_)
            | Job::Credential(_)
            | Job::AuthorityRead(_)
            | Job::UnknownCount(_)
            | Job::Continuation(_)
            | Job::Receipt(_)
            | Job::ReceiptRead(_)
            | Job::OrderedIntent(_) => None,
            #[cfg(test)]
            Job::Escalate { .. } | Job::Archive { .. } => None,
            #[cfg(feature = "core")]
            Job::Eval(_)
            | Job::Settlement(_)
            | Job::TrajectoryBegin(_)
            | Job::TrajectoryTerminal(_) => None,
        };
        let accepting = self
            .accepting
            .lock()
            .map_err(|_| Failure::new(ErrorCode::AuthorityUnavailable, operation.clone()))?;
        if !*accepting || !self.live.load(Ordering::Acquire) {
            return Err(Failure::new(ErrorCode::AuthorityUnavailable, operation));
        }
        let queue = if safety { &self.safety } else { &self.commands };
        queue.try_send(job).map_err(|error| {
            Failure::new(
                match error {
                    mpsc::error::TrySendError::Full(_) => ErrorCode::CapacityExceeded,
                    mpsc::error::TrySendError::Closed(_) => ErrorCode::AuthorityUnavailable,
                },
                operation,
            )
        })
    }

    async fn operation(
        &self,
        caller: &Caller,
        id: &OperationId,
    ) -> Result<OperationResult, Failure> {
        let identity = self
            .authority_read(actor::AuthoritySubject::Caller(caller.clone()))
            .await?;
        let (installation, principal) = self
            .authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                Ok((
                    view.facts().installation.clone(),
                    view.facts().principal(caller, Utc::now())?.clone(),
                ))
            })
            .map_err(Failure::from)?;
        let _read = self.read_permit()?;
        let operation = tokio::time::timeout(
            MANAGEMENT_TIMEOUT,
            store::operation(&self.db, &installation, &principal, id),
        )
        .await
        .map_err(|_| self.unavailable())?
        .map_err(|_| self.unavailable())?
        .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
        self.authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                let current = view.facts();
                current
                    .authorize_read(caller, &operation.context, Utc::now())
                    .map_err(|_| ErrorCode::Forbidden)?;
                if operation
                    .result
                    .authority_revision
                    .is_some_and(|revision| revision > current.revision)
                {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                Ok(())
            })
            .map_err(Failure::from)?;
        if operation.created_at + chrono::Duration::days(30) <= Utc::now() {
            return Err(Failure::new(ErrorCode::OperationExpired, Some(id.clone())));
        }
        Ok(operation.result)
    }

    async fn credential(
        &self,
        caller: &Caller,
        tenant: &TenantId,
        id: &CredentialId,
    ) -> Result<CredentialStatus, Failure> {
        let identity = self
            .authority_read(actor::AuthoritySubject::Caller(caller.clone()))
            .await?;
        let deadline = tokio::time::Instant::now() + MANAGEMENT_TIMEOUT;
        let permit = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let (reply, receive) = oneshot::channel();
        self.enqueue(
            Job::Credential(CredentialRead {
                caller: caller.clone(),
                authority: identity.lease.clone(),
                id: id.clone(),
                deadline,
                permit,
                reply,
            }),
            false,
        )?;
        // The HTTP deadline stops waiting, never the actor's transaction cleanup.
        let (observed_revision, target) = match tokio::time::timeout_at(deadline, receive).await {
            Ok(Ok(result)) => result?,
            _ => return Err(ErrorCode::AuthorityUnavailable.into()),
        };
        let target = target
            .filter(|credential| &credential.tenant == tenant)
            .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
        let revision = self
            .authority
            .read()
            .map_err(Failure::from)?
            .check(&identity.lease, |view| {
                let state = view.facts();
                if state.revision != observed_revision {
                    return Err(ErrorCode::AuthorityUnavailable);
                }
                if target.audit_reader && !matches!(caller, Caller::Installation(_)) {
                    return Err(ErrorCode::Forbidden);
                }
                state.authorize(
                    caller,
                    tenant,
                    Permission::CredentialManage,
                    &target.scope,
                    Utc::now(),
                )?;
                Ok(state.revision)
            })
            .map_err(Failure::from)?;
        let lifecycle =
            if target.state == CredentialState::Active && target.expires.instant <= Utc::now() {
                CredentialState::Expired
            } else {
                target.state
            };
        Ok(CredentialStatus {
            tenant_id: tenant.clone(),
            credential_id: id.clone(),
            credential_revision: target.revision,
            authority_revision: revision,
            state: lifecycle,
        })
    }
}

fn operator_principal(id: &str) -> Result<PrincipalId, Failure> {
    PrincipalId::for_control_credential(id)
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))
}
