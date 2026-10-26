//! Opt-in local resource authority. All authorization and storage stay in the host.

mod actor;
mod admission;
mod authority;
mod cache;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod capture;
mod command;
mod configuration;
mod configuration_store;
pub(crate) mod continuation;
mod error;
#[cfg(feature = "core")]
pub(crate) mod eval;
pub(crate) mod execution;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod hosts;
pub(crate) mod http;
mod model;
mod mutation;
pub(crate) mod oauth;
pub(crate) mod ownership;
pub(crate) mod receipts;
mod resources;
#[cfg(all(feature = "core", feature = "harness"))]
pub(crate) mod session;
mod store;
#[cfg(test)]
mod tests;
#[cfg(feature = "core")]
pub(crate) mod trajectory;
pub(super) mod trajectory_profile;
mod transaction;

use std::sync::{
    Arc, RwLock,
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

const MANAGEMENT_TIMEOUT: Duration = Duration::from_secs(5);

// The startup test installs H's existing fault gate before the actor can run.
// No global hook or alternate transaction implementation is involved.
#[cfg(test)]
struct BootstrapGate {
    phase: transaction::Phase,
    attached: oneshot::Sender<(Arc<transaction::Proof>, Arc<transaction::Gate>)>,
}

#[derive(Clone)]
pub(crate) struct Service {
    oauth: Arc<oauth::State>,
    db: DatabaseConnection,
    snapshot: Arc<RwLock<Arc<Snapshot>>>,
    live: Arc<AtomicBool>,
    instance: InstanceId,
    commands: mpsc::Sender<Job>,
    safety: mpsc::Sender<Job>,
    completions: mpsc::Sender<Job>,
    receipt_volume: Option<std::path::PathBuf>,
    stop: CancellationToken,
    accepting: Arc<std::sync::Mutex<bool>>,
    cleanup: Arc<transaction::Proof>,
    #[cfg(all(feature = "core", feature = "harness"))]
    hosts: Arc<hosts::Hosts>,
    #[cfg(feature = "core")]
    eval: Arc<eval::State>,
    reads: Arc<Semaphore>,
    resources: Arc<resources::Resources>,
    native_continuation: Option<continuation::PinnedCrypto>,
    http_continuation: Arc<continuation::http::Port>,
    http_registry: crate::continuation::ContinuationRegistry,
    caches: Arc<execution::cached::Caches>,
    admission: Arc<admission::Admission>,
    executor: Arc<crate::gateway::pipeline::executor::HttpExecutor>,
    pipelines: Arc<execution::lifecycle::Registry>,
    #[cfg(test)]
    dispatch_test_ready: Arc<AtomicBool>,
    #[cfg(test)]
    receipt_credential_gate:
        Arc<std::sync::Mutex<Option<Arc<receipts::authority_test::CredentialGate>>>>,
    completion: Shared<BoxFuture<'static, Result<(), Arc<tokio::task::JoinError>>>>,
}

impl Service {
    #[cfg(test)]
    pub(crate) async fn open(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
    ) -> anyhow::Result<Self> {
        Self::open_with_limits(db, owner, actor::OperationLimits::default()).await
    }

    pub(crate) async fn open_profile(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
    ) -> anyhow::Result<Self> {
        Self::open_configured(
            db,
            owner,
            actor::OperationLimits::default(),
            config,
            continuations,
        )
        .await
    }

    #[cfg(test)]
    async fn open_with_limits(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        limits: actor::OperationLimits,
    ) -> anyhow::Result<Self> {
        Self::open_configured(
            db,
            owner,
            limits,
            &crate::config::tenancy::TenancyConfig::default(),
            &crate::config::ContinuationConfig::default(),
        )
        .await
    }

    async fn open_configured(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        limits: actor::OperationLimits,
        config: &crate::config::tenancy::TenancyConfig,
        continuations: &crate::config::ContinuationConfig,
    ) -> anyhow::Result<Self> {
        let initial = store::load(&db, Utc::now()).await?;
        let resources = Arc::new(resources::Resources::load_key(
            &initial.installation,
            config,
        )?);
        let import = resources::Resources::read_import(config.resource_import.as_deref()).await?;
        let service = Self::start_resources_with_continuations(
            db,
            owner,
            limits,
            initial,
            resources,
            (import, continuations),
            #[cfg(test)]
            None,
        )
        .await?;
        if service.http_continuation.state.enabled() {
            service.http_registry.start_reconciler();
        }
        Ok(service)
    }

    #[cfg(test)]
    async fn start_prepared(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        limits: actor::OperationLimits,
        initial: Snapshot,
        resources: Arc<resources::Resources>,
    ) -> anyhow::Result<Self> {
        Self::start_resources(db, owner, limits, initial, resources, None, None).await
    }

    #[cfg(test)]
    async fn start_resources(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        limits: actor::OperationLimits,
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
            limits,
            initial,
            resources,
            (import, &crate::config::ContinuationConfig::default()),
            bootstrap_gate,
        )
        .await
    }

    async fn start_resources_with_continuations(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        limits: actor::OperationLimits,
        initial: Snapshot,
        resources: Arc<resources::Resources>,
        provisioning: (Option<Vec<u8>>, &crate::config::ContinuationConfig),
        #[cfg(test)] bootstrap_gate: Option<BootstrapGate>,
    ) -> anyhow::Result<Self> {
        let (import, continuations) = provisioning;
        // Construct fallible transport state before spawning the owning actor.
        // Tenant profiles share connection pools, never host credential plugins.
        let executor = Arc::new(crate::gateway::pipeline::executor::HttpExecutor::with_defaults()?);
        let caches = Arc::new(execution::cached::Caches::new()?);
        let oauth = Arc::new(oauth::State::default());
        let snapshot = Arc::new(RwLock::new(Arc::new(initial)));
        let live = Arc::new(AtomicBool::new(true));
        let cleanup = transaction::Proof::new(live.clone());
        #[cfg(test)]
        if let Some(bootstrap_gate) = bootstrap_gate {
            let gate = cleanup.hold(bootstrap_gate.phase);
            bootstrap_gate
                .attached
                .send((cleanup.clone(), gate))
                .map_err(|_| anyhow::anyhow!("bootstrap test gate receiver unavailable"))?;
        }
        let instance: InstanceId = uuid::Uuid::new_v4().to_string().try_into()?;
        // Ordinary traffic cannot occupy the queue reserved for installation
        // revokes and stop transitions. Neither queue waits for free capacity.
        let (commands, receiver) = mpsc::channel(16);
        let (safety, safety_receiver) = mpsc::channel(8);
        let (completions, completion_receiver) = mpsc::channel(128);
        let receipt_volume = owner
            .as_ref()
            .and_then(|owner| owner.path.parent().map(std::path::Path::to_path_buf));
        let receipt_installation = snapshot
            .read()
            .map_err(|_| anyhow::anyhow!("receipt startup authority unavailable"))?
            .installation
            .clone();
        let receipt_inventory = tokio::time::timeout(
            MANAGEMENT_TIMEOUT,
            receipts::store::inventory(&db, &receipt_installation),
        )
        .await
        .map_err(|_| anyhow::anyhow!("receipt inventory deadline elapsed"))??;
        let admission = admission::Admission::new();
        admission
            .initialize_receipts(
                receipt_inventory,
                receipts::capacity::free_space(receipt_volume.as_deref())?,
            )
            .map_err(|_| anyhow::anyhow!("receipt capacity initialization failed"))?;
        let accepting = Arc::new(std::sync::Mutex::new(true));
        #[cfg(feature = "core")]
        let eval = eval::State::new(live.clone());
        let http_state = crate::continuation::tenant::state::State::new(
            continuations,
            instance.as_str().to_owned(),
        )?;
        let reads = Arc::new(Semaphore::new(8));
        let http_continuation = continuation::http::Port::new(
            commands.clone(),
            &cleanup,
            reads.clone(),
            http_state.clone(),
        );
        let http_registry =
            crate::continuation::ContinuationRegistry::for_tenant(http_continuation.clone())?;
        let worker = Worker {
            oauth: oauth.clone(),
            db: db.clone(),
            snapshot: snapshot.clone(),
            live: Liveness(live.clone()),
            owner,
            limits,
            resources: resources.clone(),
            proof: cleanup.clone(),
            admission: admission.clone(),
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
            oauth,
            db,
            snapshot,
            live,
            instance,
            commands,
            safety,
            completions,
            receipt_volume,
            stop,
            accepting,
            #[cfg(feature = "core")]
            eval,
            cleanup,
            #[cfg(all(feature = "core", feature = "harness"))]
            hosts: Arc::new(hosts::Hosts::new()),
            reads,
            resources,
            native_continuation: None,
            http_continuation,
            http_registry,
            caches,
            admission,
            executor,
            pipelines: execution::lifecycle::Registry::new(),
            #[cfg(test)]
            dispatch_test_ready: Arc::new(AtomicBool::new(false)),
            #[cfg(test)]
            receipt_credential_gate: Arc::default(),
            completion,
        };
        match initialization.await {
            Ok(Ok(crypto)) => {
                service.native_continuation = crypto;
                Ok(service)
            }
            _ => {
                service.shutdown().await?;
                anyhow::bail!("native continuation key initialization failed")
            }
        }
    }

    pub(crate) fn continuation_registry(&self) -> &crate::continuation::ContinuationRegistry {
        &self.http_registry
    }

    /// Stop accepting mutations and drain both admitted command queues. Timeout
    /// never aborts the writer or releases its file lease; the host must report
    /// unsuccessful shutdown instead of claiming all transactions settled.
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        *self
            .accepting
            .lock()
            .map_err(|_| anyhow::anyhow!("tenant ingress lock unavailable"))? = false;
        {
            let _authority = self
                .snapshot
                .write()
                .map_err(|_| anyhow::anyhow!("tenant authority lock unavailable at shutdown"))?;
            self.caches.close();
        }
        self.admission.close();
        #[cfg(all(feature = "core", feature = "harness"))]
        self.hosts.close();
        let draining = async {
            #[cfg(all(feature = "core", feature = "harness"))]
            tokio::time::timeout(Duration::from_secs(30), self.hosts.drain())
                .await
                .map_err(|_| {
                    anyhow::anyhow!("native host cleanup did not drain; owner retained")
                })??;
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
        self.snapshot
            .read()
            .map(|snapshot| snapshot.clone())
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))
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

    fn authenticate(&self, token: &str) -> Result<Caller, Failure> {
        if !token.starts_with("brtk_") || token.len() > 512 {
            return Err(ErrorCode::Unauthorized.into());
        }
        let state = self.current()?;
        let hash = crate::auth::keys::hash_key(token);
        let id = state
            .credential_hashes
            .get(&hash)
            .ok_or_else(|| Failure::from(ErrorCode::Unauthorized))?;
        let caller = Caller::Credential(id.clone());
        state
            .credential(&caller, Utc::now())
            .map_err(Failure::from)?;
        Ok(caller)
    }

    fn check_installation(&self, expected: &InstallationId) -> Result<(), Failure> {
        if &self.current()?.installation == expected {
            Ok(())
        } else {
            Err(ErrorCode::CapabilityMismatch.into())
        }
    }

    fn capabilities(&self, caller: &Caller) -> Result<TenantCapabilities, Failure> {
        let state = self.current()?;
        state.principal(caller, Utc::now()).map_err(Failure::from)?;
        // No tenant inference is advertised until immutable dispatch and durable
        // strict receipts have been integrated and qualified together.
        let dispatch_ready = self.dispatch_ready(&state);
        Ok(TenantCapabilities {
            contract_version: 1,
            installation_id: state.installation.clone(),
            instance_id: self.instance.clone(),
            capability_revision: self.capability_revision(&state).map_err(Failure::from)?,
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
            Job::Native(_) | Job::Capture(_) => None,
            Job::Command(message) => Some(message.command.operation_id().clone()),
            Job::OAuth(_)
            | Job::Credential(_)
            | Job::Continuation(_)
            | Job::Receipt(_)
            | Job::ReceiptRead(_) => None,
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
        let _read = self.read_permit()?;
        let state = self.current()?;
        let principal = state
            .principal(caller, Utc::now())
            .map_err(Failure::from)?
            .clone();
        let operation = tokio::time::timeout(
            MANAGEMENT_TIMEOUT,
            store::operation(&self.db, &state.installation, &principal, id),
        )
        .await
        .map_err(|_| self.unavailable())?
        .map_err(|_| self.unavailable())?
        .ok_or_else(|| Failure::from(ErrorCode::NotFound))?;
        // Do not carry the pre-I/O snapshot across this authorization point.
        let current = self.current()?;
        current
            .authorize_read(caller, &operation.context, Utc::now())
            .map_err(|_| Failure::from(ErrorCode::Forbidden))?;
        if operation
            .result
            .authority_revision
            .is_some_and(|revision| revision > current.revision)
        {
            // A durable commit may be visible to another pool connection before
            // the writer publishes its snapshot. Do not acknowledge it early.
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
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
        self.current()?
            .principal(caller, Utc::now())
            .map_err(Failure::from)?;
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
        let state = self.current()?;
        if state.revision != observed_revision {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        if target.audit_reader && !matches!(caller, Caller::Installation(_)) {
            return Err(ErrorCode::Forbidden.into());
        }
        state
            .authorize(
                caller,
                tenant,
                Permission::CredentialManage,
                &target.scope,
                Utc::now(),
            )
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
            authority_revision: state.revision,
            state: lifecycle,
        })
    }
}

fn operator_principal(id: &str) -> Result<PrincipalId, Failure> {
    PrincipalId::for_control_credential(id)
        .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))
}
