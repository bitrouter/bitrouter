//! Opt-in local resource authority. All authorization and storage stay in the host.

mod actor;
mod authority;
mod command;
mod error;
pub(crate) mod http;
mod model;
mod mutation;
pub(crate) mod ownership;
mod store;
#[cfg(test)]
mod tests;

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
use tokio::sync::{mpsc, oneshot};
use tokio_util::sync::CancellationToken;

use self::{
    actor::{Liveness, Message, Worker},
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
    commands: mpsc::Sender<Message>,
    safety: mpsc::Sender<Message>,
    stop: CancellationToken,
    completion: Shared<BoxFuture<'static, Result<(), Arc<tokio::task::JoinError>>>>,
}

impl Service {
    pub(crate) async fn open(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
    ) -> anyhow::Result<Self> {
        Self::open_with_limits(db, owner, actor::OperationLimits::default()).await
    }

    async fn open_with_limits(
        db: DatabaseConnection,
        owner: Option<ownership::Lease>,
        limits: actor::OperationLimits,
    ) -> anyhow::Result<Self> {
        let initial = store::load(&db, Utc::now()).await?;
        let snapshot = Arc::new(RwLock::new(Arc::new(initial)));
        let live = Arc::new(AtomicBool::new(true));
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
            completion,
        })
    }

    /// Stop accepting mutations and drain both admitted command queues. Timeout
    /// never aborts the writer or releases its file lease; the host must report
    /// unsuccessful shutdown instead of claiming all transactions settled.
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.stop.cancel();
        tokio::time::timeout(Duration::from_secs(30), self.completion.clone())
            .await
            .map_err(|_| {
                anyhow::anyhow!("tenant authority shutdown did not settle within 30 seconds")
            })?
            .map_err(|_| anyhow::anyhow!("tenant authority worker ended unexpectedly"))
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
        Ok(TenantCapabilities {
            contract_version: 1,
            installation_id: state.installation.clone(),
            instance_id: self.instance.clone(),
            capability_revision: format!("{}-{}", self.instance, u8::from(state.saturated))
                .try_into()
                .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?,
            revision_bound_dispatch: false,
            strict_receipts: false,
            dispatch_enabled: false,
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
        let queue = if safety { &self.safety } else { &self.commands };
        queue.try_send(message).map_err(|error| match error {
            mpsc::error::TrySendError::Full(_) => {
                Failure::new(ErrorCode::CapacityExceeded, Some(operation.clone()))
            }
            mpsc::error::TrySendError::Closed(_) => {
                Failure::new(ErrorCode::AuthorityUnavailable, Some(operation.clone()))
            }
        })?;
        match tokio::time::timeout(MANAGEMENT_TIMEOUT, receive).await {
            Ok(Ok(response)) => response,
            _ => Err(Failure::new(
                ErrorCode::AuthorityUnavailable,
                Some(operation),
            )),
        }
    }

    async fn operation(
        &self,
        caller: &Caller,
        id: &OperationId,
    ) -> Result<OperationResult, Failure> {
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
        let (observed_revision, target) = tokio::time::timeout(
            MANAGEMENT_TIMEOUT,
            store::credential_view(&self.db, id.as_str()),
        )
        .await
        .map_err(|_| self.unavailable())?
        .map_err(|_| self.unavailable())?;
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
