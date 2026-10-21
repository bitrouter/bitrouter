//! Shared OAuth cancellation participates in real request/generation ownership.

use super::*;
use async_trait::async_trait;
use std::sync::atomic::{AtomicUsize, Ordering};
use tokio::io::AsyncWriteExt as _;
use tokio::sync::Semaphore;

use crate::gateway::model::error::{ModelError, Result as ModelResult};
use crate::gateway::providers::auth::{
    credentials::{Credential, OAuthToken},
    store::{
        CredentialKey, CredentialStore, CredentialTransaction, Durability, OAuthRefresher,
        OAuthSession, StoreError,
    },
};
use crate::gateway::shared::{
    credentials::{Credentials, observation::Observation},
    store::{Kind, Stored},
    test_database::combine_results,
};

struct Probe {
    calls: AtomicUsize,
    returned: AtomicUsize,
    refresh_dropped: AtomicUsize,
    transactions: AtomicUsize,
    transaction_dropped: AtomicUsize,
    entered: Notify,
    ended: Notify,
    release: Semaphore,
}

impl Probe {
    fn new() -> Self {
        Self {
            calls: AtomicUsize::new(0),
            returned: AtomicUsize::new(0),
            refresh_dropped: AtomicUsize::new(0),
            transactions: AtomicUsize::new(0),
            transaction_dropped: AtomicUsize::new(0),
            entered: Notify::new(),
            ended: Notify::new(),
            release: Semaphore::new(0),
        }
    }
}

struct RefreshDrop<'a>(&'a Probe);

impl Drop for RefreshDrop<'_> {
    fn drop(&mut self) {
        self.0.refresh_dropped.fetch_add(1, Ordering::SeqCst);
    }
}

#[async_trait]
impl OAuthRefresher for Probe {
    async fn refresh(&self, _: &OAuthToken) -> ModelResult<OAuthToken> {
        let _drop = RefreshDrop(self);
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        let permit = self
            .release
            .acquire()
            .await
            .map_err(|_| ModelError::configuration("fixture release closed"))?;
        permit.forget();
        self.returned.fetch_add(1, Ordering::SeqCst);
        Ok(token("must-not-be-committed", 0))
    }
}

// Observe destruction without replacing any shared store/fence operation. This
// also lets cleanup wait for the old detached implementation on a failing run.
struct ObservedStore {
    credentials: Credentials,
    probe: Arc<Probe>,
}

struct ObservedTransaction {
    inner: Box<dyn CredentialTransaction>,
    // Fields drop in declaration order: signal after the real transaction.
    _drop: TransactionDrop,
}

struct TransactionDrop(Arc<Probe>);

impl Drop for TransactionDrop {
    fn drop(&mut self) {
        self.0.transaction_dropped.fetch_add(1, Ordering::SeqCst);
        self.0.ended.notify_one();
    }
}

#[async_trait]
impl CredentialStore for ObservedStore {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> std::result::Result<Box<dyn CredentialTransaction>, StoreError> {
        let inner = self.credentials.begin(key).await?;
        self.probe.transactions.fetch_add(1, Ordering::SeqCst);
        Ok(Box::new(ObservedTransaction {
            inner,
            _drop: TransactionDrop(self.probe.clone()),
        }))
    }
}

#[async_trait]
impl CredentialTransaction for ObservedTransaction {
    fn credential(&self) -> Option<&Credential> {
        self.inner.credential()
    }
    fn pending(&self) -> Option<&Credential> {
        self.inner.pending()
    }
    fn stage(&mut self, credential: Credential) {
        self.inner.stage(credential);
    }
    async fn prepare_refresh(&mut self) -> std::result::Result<(), StoreError> {
        self.inner.prepare_refresh().await
    }
    async fn commit(&mut self) -> std::result::Result<Durability, StoreError> {
        self.inner.commit().await
    }
    async fn release(&mut self) -> std::result::Result<(), StoreError> {
        self.inner.release().await
    }
}

fn token(access: &str, expiry: u64) -> OAuthToken {
    OAuthToken {
        access_token: access.into(),
        expires_at: expiry,
        refresh_token: Some("fixture-original-refresh".into()),
    }
}

async fn session(
    fixture: &Fixture,
    probe: &Arc<Probe>,
    expiry: u64,
) -> Result<(OAuthSession, CredentialKey, String)> {
    let pool = uuid::Uuid::new_v4().to_string();
    fixture
        .lifecycle
        .engine
        .set_resource(&pool, None, 1)
        .await?;
    let key = CredentialKey {
        provider: "claude-code".into(),
        account: pool.clone(),
    };
    let credentials = Credentials::new(fixture.authority.clone());
    let stamp = credentials
        .import(
            &key,
            &pool,
            None,
            &Credential::Oauth(token("original", expiry)),
        )
        .await?;
    Ok((
        OAuthSession::new_inline(
            Arc::new(ObservedStore {
                credentials,
                probe: probe.clone(),
            }),
            probe.clone(),
            Duration::from_secs(30),
        ),
        key,
        stamp.id,
    ))
}

async fn started(fixture: &Fixture, id: &str) -> Result<serde_json::Value> {
    let row = fixture
        .authority
        .store()
        .get::<serde_json::Value>(Kind::Credential, id)
        .await?
        .context("credential missing")?;
    ensure!(row.value["lease"]["started"] == true);
    ensure!(row.value["stamp"]["version"] == 1);
    Ok(row.value)
}

async fn remains_fenced(
    fixture: &Fixture,
    id: &str,
    key: &CredentialKey,
    original: &serde_json::Value,
    probe: &Arc<Probe>,
) -> Result<()> {
    let store = fixture.authority.store();
    ensure!(started(fixture, id).await? == *original);
    let tx = store.begin().await?;
    let mut row: Stored<serde_json::Value> = store.lock(&tx, Kind::Credential, id).await?;
    // Expiry is a fixture mutation, never permission to replay a started token.
    row.value["lease"]["expires_at_ms"] = 0.into();
    let expected = row.value.clone();
    store
        .save(&tx, &mut row, "refreshing", None, Store::now(&tx).await?)
        .await?;
    tx.commit().await?;

    let credentials = Credentials::new(fixture.authority.clone());
    ensure!(matches!(
        credentials.begin(key).await,
        Err(StoreError::Conflict)
    ));
    ensure!(credentials.load(key).await.is_err());
    let retry = OAuthSession::new_inline(
        Arc::new(credentials),
        probe.clone(),
        Duration::from_secs(30),
    );
    for result in [
        retry.resolve(key).await,
        retry.recover(key, Some("original")).await,
    ] {
        ensure!(matches!(
            result,
            Err(ModelError::CredentialStorage {
                failure: StoreError::Conflict
            })
        ));
    }
    ensure!(probe.calls.load(Ordering::SeqCst) == 1);
    ensure!(probe.returned.load(Ordering::SeqCst) == 0);
    ensure!(probe.refresh_dropped.load(Ordering::SeqCst) == 1);
    ensure!(probe.transaction_dropped.load(Ordering::SeqCst) == 1);
    ensure!(started(fixture, id).await? == expected);
    Ok(())
}

// The lifecycle, probes and pools remain outside the campaign timeout. Join
// transport/native/pipeline/workers/telemetry before closing either pool. Keep
// both the primary assertion and every cleanup failure in the returned error.
async fn finish(mut fixture: Fixture, probe: Arc<Probe>, result: Result<()>) -> Result<()> {
    let deadline = fixture.lifecycle.shutdown.as_ref().map_or_else(
        || Instant::now() + Duration::from_secs(2),
        |state| state.deadline,
    );
    let force = CancellationToken::new();
    force.cancel();
    let joined = tokio::time::timeout(
        Duration::from_secs(5),
        fixture.lifecycle.shutdown_until(deadline, &force),
    )
    .await
    .context("OAuth fixture lifecycle cleanup stalled")
    .and_then(|report| report)
    .and_then(|report| {
        ensure!(report.quiescent, "OAuth fixture still owns roots");
        Ok(())
    });
    // On the unfixed implementation this releases the escaped refresh and
    // observes destruction of its transaction before pools are closed. It is
    // never used to satisfy the cancellation assertions in the campaign.
    probe.release.add_permits(1);
    let oauth = tokio::time::timeout(Duration::from_secs(2), async {
        while probe.transaction_dropped.load(Ordering::SeqCst)
            != probe.transactions.load(Ordering::SeqCst)
        {
            probe.ended.notified().await;
        }
    })
    .await
    .context("OAuth fixture transaction remained alive");
    drop(fixture.lifecycle);
    let heartbeat = tokio::time::timeout(Duration::from_secs(3), fixture.heartbeat.close())
        .await
        .context("OAuth fixture heartbeat pool close stalled")
        .and_then(|result| result.map_err(anyhow::Error::from));
    let main = tokio::time::timeout(Duration::from_secs(3), fixture.main.close())
        .await
        .context("OAuth fixture main pool close stalled")
        .and_then(|result| result.map_err(anyhow::Error::from));
    combine_results(result, vec![joined, oauth, heartbeat, main])
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn force_joins_shared_oauth_request_before_generation_and_telemetry() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let probe = Arc::new(Probe::new());
    let result = tokio::time::timeout(Duration::from_secs(15), async {
        fixture.start().await?;
        let (session, key, id) = session(&fixture, &probe, 1).await?;
        let serving = fixture.lifecycle.generations.serving().clone();
        let snapshot = serving.snapshot()?;
        let owner = snapshot.owner;
        let tasks = snapshot.tasks().clone();
        drop(snapshot);
        let handler_key = key.clone();
        let router = Router::new().route(
            "/held-oauth",
            get(move || {
                let serving = serving.clone();
                let session = session.clone();
                let key = handler_key.clone();
                async move {
                    // The real pre-extractor source and custody stay with this
                    // transport-owned request until its OAuth future is dropped.
                    let Ok(owners) = serving.acquire_request() else {
                        return StatusCode::SERVICE_UNAVAILABLE;
                    };
                    let _result = session.resolve(&key).await;
                    drop(owners);
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
        let mut socket = tokio::net::TcpStream::connect(address).await?;
        socket
            .write_all(b"GET /held-oauth HTTP/1.1\r\nHost: localhost\r\n\r\n")
            .await?;
        tokio::time::timeout(Duration::from_secs(2), probe.entered.notified()).await?;
        let original = started(&fixture, &id).await?;
        ensure!(tasks.pending().sources == 1);
        ensure!(
            fixture
                .lifecycle
                .generations
                .serving()
                .custody
                .pending_for(owner)
                == 1
        );

        let deadline = Instant::now() + Duration::from_secs(3);
        let force = CancellationToken::new();
        tokio::time::timeout(Duration::from_secs(1), async {
            loop {
                {
                    let wait = fixture.lifecycle.shutdown_until(deadline, &force);
                    tokio::pin!(wait);
                    if let std::task::Poll::Ready(result) = futures::poll!(&mut wait) {
                        result?;
                        anyhow::bail!("shutdown completed while OAuth was held");
                    }
                }
                let state = fixture
                    .lifecycle
                    .shutdown
                    .as_ref()
                    .context("shutdown missing")?;
                ensure!(state.deadline == deadline && !state.forced);
                ensure!(!state.telemetry_joined && !fixture.lifecycle.report().quiescent);
                if let Some(attempts) = &state.attempts {
                    ensure!(attempts.iter().any(|(id, _)| *id == owner));
                    ensure!(state.retired.is_empty());
                    return Ok::<(), anyhow::Error>(());
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .context("held OAuth retirement attempt was not observed")??;
        ensure!(tasks.pending().sources == 1);
        ensure!(probe.refresh_dropped.load(Ordering::SeqCst) == 0);
        ensure!(probe.transaction_dropped.load(Ordering::SeqCst) == 0);
        ensure!(
            fixture
                .lifecycle
                .shutdown_until(deadline + Duration::from_secs(1), &force)
                .await
                .is_err()
        );

        fixture.lifecycle.force();
        // This current-thread test has not yielded since the force broadcast.
        // A cancelled join waiter must retain the connection and cannot advance
        // telemetry before destroying the refresh and transaction underneath it.
        {
            let wait = fixture.lifecycle.join_forced();
            tokio::pin!(wait);
            ensure!(futures::poll!(&mut wait).is_pending());
        }
        ensure!(probe.refresh_dropped.load(Ordering::SeqCst) == 0);
        ensure!(
            !fixture
                .lifecycle
                .shutdown
                .as_ref()
                .context("shutdown missing")?
                .telemetry_joined
        );
        let report = fixture.lifecycle.shutdown_until(deadline, &force).await?;
        ensure!(report.forced && report.quiescent && !report.clean && !report.durable_resolved);
        ensure!(probe.refresh_dropped.load(Ordering::SeqCst) == 1);
        ensure!(probe.transaction_dropped.load(Ordering::SeqCst) == 1);
        ensure!(probe.returned.load(Ordering::SeqCst) == 0);
        let transport = fixture
            .lifecycle
            .transport
            .as_ref()
            .context("transport missing")?
            .report();
        ensure!(
            transport.is_quiescent()
                && transport.connections.joined == 1
                && transport.connections.cancelled == 1
        );
        let generations = fixture.lifecycle.generations.forced_reports();
        ensure!(generations.len() == 1 && generations[0].generation == owner);
        ensure!(generations[0].is_quiescent() && !generations[0].pipeline_failed);
        ensure!(tasks.pending().sources == 0);
        ensure!(
            fixture
                .lifecycle
                .shutdown
                .as_ref()
                .context("shutdown missing")?
                .telemetry_joined
        );
        remains_fenced(&fixture, &id, &key, &original, &probe).await?;
        drop(socket);
        Ok(())
    })
    .await
    .context("OAuth force campaign timed out")
    .and_then(|result| result);
    finish(fixture, probe, result).await
}

#[tokio::test]
#[ignore = "requires PostgreSQL via BITROUTER_TEST_DATABASE_URL"]
async fn cancelling_shared_rejected_token_recovery_preserves_started_fence() -> Result<()> {
    let fixture = Fixture::new().await?;
    let probe = Arc::new(Probe::new());
    let result = tokio::time::timeout(Duration::from_secs(10), async {
        // A valid stored token exercises recover(), rather than expiry refresh.
        let (session, key, id) = session(&fixture, &probe, 0).await?;
        let observation = Observation::default();
        let mut call = Box::pin(observation.scope(session.recover(&key, Some("original"))));
        tokio::select! {
            result = &mut call => {
                result?;
                anyhow::bail!("held OAuth recovery completed early");
            }
            () = probe.entered.notified() => {}
        }
        let original = started(&fixture, &id).await?;
        drop(call);
        // No yield or task-count inference: destruction is synchronous with
        // cancellation of the actual inline recovery future.
        ensure!(probe.refresh_dropped.load(Ordering::SeqCst) == 1);
        ensure!(probe.transaction_dropped.load(Ordering::SeqCst) == 1);
        ensure!(observation.finish().is_err());
        remains_fenced(&fixture, &id, &key, &original, &probe).await
    })
    .await
    .context("OAuth recovery campaign timed out")
    .and_then(|result| result);
    finish(fixture, probe, result).await
}
