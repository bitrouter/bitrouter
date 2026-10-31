//! Retired installation receipt reader. No Service, writer, maintenance or inference.
mod connection;
mod http;
mod listener;
pub(crate) mod recovery;
#[cfg(all(test, unix))]
pub(super) mod tests;

use super::{
    error::Failure,
    model::{Caller, Snapshot},
    receipts::read,
};
use crate::offline::engine::Connections;
use bitrouter_ai::tenancy::{identity::TenantId, operation::ErrorCode};
use connection::ReadConnection;
use futures::{
    FutureExt,
    future::{BoxFuture, Shared},
};
use std::{panic::AssertUnwindSafe, sync::Arc, time::Duration};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc, oneshot};
use tokio_util::sync::CancellationToken;

const READS: usize = 8;
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
const CLEANUP: Duration = Duration::from_secs(30);

struct Job {
    caller: Caller,
    tenant: TenantId,
    request: read::Request,
    deadline: tokio::time::Instant,
    cancel: CancellationToken,
    permit: OwnedSemaphorePermit,
    reply: oneshot::Sender<Result<Reply, Failure>>,
    #[cfg(all(test, unix))]
    hooks: Arc<tests::Hooks>,
}
pub(super) struct Reply {
    bytes: Vec<u8>,
    _permit: OwnedSemaphorePermit,
}
impl AsRef<[u8]> for Reply {
    fn as_ref(&self) -> &[u8] {
        &self.bytes
    }
}
#[derive(Clone)]
pub(crate) struct Archive {
    state: Arc<Snapshot>,
    reads: Arc<Semaphore>,
    sender: mpsc::Sender<Job>,
    stop: CancellationToken,
    closed: Shared<BoxFuture<'static, bool>>,
    #[cfg(all(test, unix))]
    hooks: Arc<tests::Hooks>,
}

/// The caller owns Connections before establishment and remains its sole owner.
pub(crate) struct Loaded(Snapshot);

pub(crate) async fn load_existing(connections: &mut Connections) -> anyhow::Result<Loaded> {
    let db = connections
        .copy
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("archive connection missing"))?;
    sea_orm::sqlx::query("PRAGMA query_only=ON")
        .execute(&mut *db)
        .await?;
    connections.uncertain = true;
    sea_orm::sqlx::query("BEGIN").execute(&mut *db).await?;
    let loaded = async {
        let reader = ReadConnection::new(db);
        // A valid ledger/digest does not imply BINARY key comparisons. Validate
        // the exact receipt schema before creating any serving Archive.
        crate::db::migration::validate_receipt_archive(&reader).await?;
        let state = super::store::load_existing(&reader, chrono::Utc::now()).await?;
        super::receipts::store::inventory(&reader, &state.installation).await?;
        Ok::<_, anyhow::Error>(state)
    }
    .await;
    connections.cleanup_budget().await?;
    let db = connections
        .copy
        .as_mut()
        .ok_or_else(|| anyhow::anyhow!("archive connection missing"))?;
    sea_orm::sqlx::query("ROLLBACK").execute(&mut *db).await?;
    connections.uncertain = false;
    loaded.map(Loaded)
}
impl Archive {
    pub(crate) fn installation(&self) -> &bitrouter_ai::tenancy::identity::InstallationId {
        &self.state.installation
    }
    pub(crate) fn start(connections: Connections, state: Loaded, stop: CancellationToken) -> Self {
        let (sender, receiver) = mpsc::channel(READS);
        let (closed, wait) = oneshot::channel();
        let state = Arc::new(state.0);
        let worker_state = state.clone();
        let worker_stop = stop.clone();
        #[cfg(all(test, unix))]
        let hooks = Arc::new(tests::Hooks::default());
        #[cfg(all(test, unix))]
        let worker_hooks = hooks.clone();
        // The handle is intentionally not an AbortOnDropHandle. Abandoned HTTP
        // and CLI waiters signal cancellation, never drop SQL/rollback/close.
        tokio::spawn(async move {
            owner(
                connections,
                worker_state,
                receiver,
                worker_stop,
                closed,
                #[cfg(all(test, unix))]
                worker_hooks,
            )
            .await;
        });
        Self {
            state,
            #[cfg(all(test, unix))]
            hooks,
            sender,
            stop,
            reads: Arc::new(Semaphore::new(READS)),
            closed: async move { matches!(wait.await, Ok(true)) }
                .boxed()
                .shared(),
        }
    }
    async fn read(
        &self,
        caller: Caller,
        tenant: TenantId,
        request: read::Request,
    ) -> Result<Reply, Failure> {
        if self.stop.is_cancelled() {
            return Err(ErrorCode::AuthorityUnavailable.into());
        }
        read::authorize(&self.state, &caller, &tenant).map_err(Failure::from)?;
        let permit = self
            .reads
            .clone()
            .try_acquire_owned()
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        let cancel = self.stop.child_token();
        let _cancel_on_drop = cancel.clone().drop_guard();
        let deadline = tokio::time::Instant::now() + super::MANAGEMENT_TIMEOUT;
        let (reply, wait) = oneshot::channel();
        self.sender
            .try_send(Job {
                caller,
                tenant,
                request,
                cancel,
                deadline,
                permit,
                reply,
                #[cfg(all(test, unix))]
                hooks: self.hooks.clone(),
            })
            .map_err(|_| Failure::from(ErrorCode::CapacityExceeded))?;
        tokio::time::timeout_at(deadline, wait)
            .await
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
            .map_err(|_| Failure::from(ErrorCode::AuthorityUnavailable))?
    }
    pub(crate) async fn shutdown(&self) -> anyhow::Result<()> {
        self.stop.cancel();
        self.reads.close();
        anyhow::ensure!(
            tokio::time::timeout(CLEANUP, self.closed.clone())
                .await
                .is_ok_and(|clean| clean),
            "archive driver cleanup unconfirmed; owner remains retained"
        );
        Ok(())
    }
    pub(crate) async fn serve(
        &self,
        listener: tokio::net::TcpListener,
        operators: crate::remote_control::auth::ControlAuth,
    ) -> anyhow::Result<()> {
        let router = http::router(self.clone(), operators);
        let listener = listener::Bounded::new(listener, self.stop.clone());
        let serving = axum::serve(listener, router)
            .with_graceful_shutdown(self.stop.clone().cancelled_owned());
        let result = serving.await;
        let cleanup = self.shutdown().await;
        cleanup?;
        result?;
        Ok(())
    }
}
async fn owner(
    mut connections: Connections,
    state: Arc<Snapshot>,
    mut receiver: mpsc::Receiver<Job>,
    stop: CancellationToken,
    closed: oneshot::Sender<bool>,
    #[cfg(all(test, unix))] hooks: Arc<tests::Hooks>,
) {
    let result = AssertUnwindSafe(async {
        loop {
            let job = tokio::select! { biased;
                () = stop.cancelled() => break,
                job = receiver.recv() => match job { Some(job) => job, None => break },
            };
            if job.cancel.is_cancelled() || tokio::time::Instant::now() >= job.deadline {
                if job
                    .reply
                    .send(Err(ErrorCode::AuthorityUnavailable.into()))
                    .is_err()
                {
                    tracing::debug!("archive expired read abandoned");
                }
                continue;
            }
            let body = query(&mut connections, &state, &job).await;
            let unavailable = body
                .as_ref()
                .err()
                .is_some_and(|failure| failure.0.code == ErrorCode::StorageUnavailable);
            let result = body.map(|bytes| Reply {
                bytes,
                _permit: job.permit,
            });
            if job.reply.send(result).is_err() {
                tracing::debug!("archive read reply abandoned after rollback");
            }
            if unavailable {
                break;
            }
        }
    })
    .catch_unwind()
    .await;
    stop.cancel();
    receiver.close();
    while let Some(job) = receiver.recv().await {
        if job
            .reply
            .send(Err(ErrorCode::AuthorityUnavailable.into()))
            .is_err()
        {
            tracing::debug!("archive queued read abandoned");
        }
    }
    if result.is_err() {
        connections.uncertain = true;
    }
    #[cfg(all(test, unix))]
    hooks.hit(tests::Step::Close).await;
    let cleanup = AssertUnwindSafe(connections.close()).catch_unwind().await;
    let clean = matches!(cleanup, Ok(Ok(()))) && !connections.uncertain;
    drop(connections); // Unconfirmed teardown preserves the canonical lease.
    if closed.send(clean).is_err() {
        tracing::debug!("archive shutdown observer abandoned");
    }
}
async fn query(
    connections: &mut Connections,
    state: &Snapshot,
    job: &Job,
) -> Result<Vec<u8>, Failure> {
    read::authorize(state, &job.caller, &job.tenant).map_err(Failure::from)?;
    connections
        .read_budget(
            job.cancel.clone(),
            job.deadline
                .saturating_duration_since(tokio::time::Instant::now()),
        )
        .await
        .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
    connections.uncertain = true;
    let body = async {
        let db = connections
            .copy
            .as_mut()
            .ok_or_else(|| Failure::from(ErrorCode::StorageUnavailable))?;
        sea_orm::sqlx::query("BEGIN")
            .execute(&mut *db)
            .await
            .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
        #[cfg(all(test, unix))]
        job.hooks.hit(tests::Step::Read).await;
        let reader = ReadConnection::new(db);
        #[cfg(all(test, unix))]
        let reader = reader.with_test_hooks(&job.hooks);
        read::body(
            &reader,
            &state.installation,
            &job.tenant,
            &job.request,
            || Failure::from(ErrorCode::StorageUnavailable),
        )
        .await
    }
    .await;
    #[cfg(all(test, unix))]
    {
        job.hooks.observe_rollback_body(&body);
        job.hooks.hit(tests::Step::Rollback).await;
    }
    // Always disable the query cancellation policy before actual rollback. A
    // caller timeout never drops a transaction or invents a cleanup witness.
    connections
        .cleanup_budget()
        .await
        .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
    let db = connections
        .copy
        .as_mut()
        .ok_or_else(|| Failure::from(ErrorCode::StorageUnavailable))?;
    sea_orm::sqlx::query("ROLLBACK")
        .execute(&mut *db)
        .await
        .map_err(|_| Failure::from(ErrorCode::StorageUnavailable))?;
    connections.uncertain = false;
    // Acknowledged rollback proves cleanup, not archive integrity. Preserve a
    // fatal read failure for the owner even if its waiter expired/disappeared.
    if body
        .as_ref()
        .is_err_and(|failure| failure.0.code == ErrorCode::StorageUnavailable)
    {
        return body;
    }
    if job.cancel.is_cancelled() || tokio::time::Instant::now() >= job.deadline {
        return Err(ErrorCode::AuthorityUnavailable.into());
    }
    let bytes = body?;
    if bytes.len() > MAX_RESPONSE {
        return Err(ErrorCode::CapacityExceeded.into());
    }
    read::authorize(state, &job.caller, &job.tenant).map_err(Failure::from)?;
    Ok(bytes)
}
