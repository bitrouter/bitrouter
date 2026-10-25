//! Opt-in local resource authority. All authorization and storage stay in the host.

mod actor;
mod admission;
mod authority;
mod cache;
mod command;
mod configuration;
mod configuration_store;
mod error;
pub(crate) mod execution;
pub(crate) mod http;
mod model;
mod mutation;
pub(crate) mod ownership;
mod resources;
mod store;
#[cfg(test)]
mod tests;
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

#[derive(Clone)]
pub(crate) struct Service {
    db: DatabaseConnection,
    snapshot: Arc<RwLock<Arc<Snapshot>>>,
    live: Arc<AtomicBool>,
    instance: InstanceId,
    commands: mpsc::Sender<Job>,
    safety: mpsc::Sender<Job>,
    stop: CancellationToken,
    accepting: Arc<std::sync::Mutex<bool>>,
    cleanup: Arc<transaction::Proof>,
    reads: Arc<Semaphore>,
    resources: Arc<resources::Resources>,
    caches: Arc<execution::cached::Caches>,
    admission: Arc<admission::Admission>,
    executor: Arc<crate::gateway::pipeline::executor::HttpExecutor>,
    pipelines: Arc<execution::lifecycle::Registry>,
    #[cfg(test)]
    dispatch_test_ready: Arc<AtomicBool>,
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
    ) -> anyhow::Result<Self> {
        Self::open_configured(db, owner, actor::OperationLimits::default(), config).await
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
        )
        .await
    }

    async fn open_configured(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        limits: actor::OperationLimits,
        config: &crate::config::tenancy::TenancyConfig,
    ) -> anyhow::Result<Self> {
        let initial = store::load(&db, Utc::now()).await?;
        let resources =
            Arc::new(resources::Resources::open(&db, &initial.installation, config).await?);
        for (tenant, revision) in &initial.active_configurations {
            let stored = configuration_store::configuration(&db, tenant, revision)
                .await
                .map_err(|_| anyhow::anyhow!("active tenant configuration unavailable"))?
                .ok_or_else(|| anyhow::anyhow!("active tenant configuration unavailable"))?;
            resources.validate_compiled(&stored).map_err(|_| {
                anyhow::anyhow!("active tenant configuration authentication failed")
            })?;
            configuration::validate_profile(&stored.revision.document).map_err(|_| {
                anyhow::anyhow!("active tenant configuration requires unavailable features")
            })?;
        }
        Self::start_prepared(db, owner, limits, initial, resources)
    }

    fn start_prepared(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        limits: actor::OperationLimits,
        initial: Snapshot,
        resources: Arc<resources::Resources>,
    ) -> anyhow::Result<Self> {
        // Construct fallible transport state before spawning the owning actor.
        // Tenant profiles share connection pools, never host credential plugins.
        let executor = Arc::new(crate::gateway::pipeline::executor::HttpExecutor::with_defaults()?);
        let caches = Arc::new(execution::cached::Caches::new()?);
        let snapshot = Arc::new(RwLock::new(Arc::new(initial)));
        let live = Arc::new(AtomicBool::new(true));
        let cleanup = transaction::Proof::new(live.clone());
        let instance = uuid::Uuid::new_v4().to_string().try_into()?;
        // Ordinary traffic cannot occupy the queue reserved for installation
        // revokes and stop transitions. Neither queue waits for free capacity.
        let (commands, receiver) = mpsc::channel(16);
        let (safety, safety_receiver) = mpsc::channel(8);
        let worker = Worker {
            db: db.clone(),
            snapshot: snapshot.clone(),
            live: Liveness(live.clone()),
            owner,
            limits,
            resources: resources.clone(),
            proof: cleanup.clone(),
        };
        let stop = CancellationToken::new();
        let worker = tokio::spawn(worker.run(receiver, safety_receiver, stop.clone()));
        let completion = async move { worker.await.map_err(Arc::new) }
            .boxed()
            .shared();
        Ok(Self {
            db,
            snapshot,
            live,
            instance,
            commands,
            safety,
            stop,
            accepting: Arc::new(std::sync::Mutex::new(true)),
            cleanup,
            reads: Arc::new(Semaphore::new(8)),
            resources,
            caches,
            admission: admission::Admission::new(),
            executor,
            pipelines: execution::lifecycle::Registry::new(),
            #[cfg(test)]
            dispatch_test_ready: Arc::new(AtomicBool::new(false)),
            completion,
        })
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
        let draining = async {
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
            Job::Command(message) => Some(message.command.operation_id().clone()),
            Job::Credential(_) => None,
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
