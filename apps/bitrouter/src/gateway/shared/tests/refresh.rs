//! Independent-pool OAuth rotation and crash ambiguity fixtures.

use crate::runtime_crypto::DeploymentKey;
use std::{
    sync::{
        Arc,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use anyhow::{Context, Result, ensure};
use async_trait::async_trait;
use serde_json::Value;
use tokio::sync::Notify;

use crate::gateway::{
    model::error::ModelError,
    providers::auth::{
        credentials::{Credential, OAuthToken},
        store::{CredentialKey, CredentialStore, OAuthRefresher, OAuthSession},
    },
    shared::{
        authority::Authority,
        credentials::Credentials,
        engine::Engine,
        store::{Kind, Store, Stored},
    },
};

struct Refresh {
    calls: AtomicUsize,
    entered: Notify,
    release: Notify,
}

struct FailedRefresh(AtomicUsize);

#[async_trait]
impl OAuthRefresher for FailedRefresh {
    async fn refresh(&self, _: &OAuthToken) -> std::result::Result<OAuthToken, ModelError> {
        self.0.fetch_add(1, Ordering::SeqCst);
        Err(ModelError::Transport {
            message: "fixture lost refresh response".into(),
        })
    }
}

#[async_trait]
impl OAuthRefresher for Refresh {
    async fn refresh(&self, _: &OAuthToken) -> std::result::Result<OAuthToken, ModelError> {
        let ordinal = self.calls.fetch_add(1, Ordering::SeqCst) + 1;
        self.entered.notify_one();
        self.release.notified().await;
        Ok(token(&format!("rotated-{ordinal}"), 0))
    }
}

fn token(access: &str, expiry: u64) -> OAuthToken {
    OAuthToken {
        access_token: access.into(),
        expires_at: expiry,
        refresh_token: Some("fixture-refresh".into()),
    }
}

async fn expire(store: &Store, id: &str) -> Result<()> {
    let tx = store.begin().await?;
    let mut account: Stored<Value> = store.lock(&tx, Kind::Credential, id).await?;
    account.value["lease"]["expires_at_ms"] = 0.into();
    store
        .save(
            &tx,
            &mut account,
            "refreshing",
            None,
            Store::now(&tx).await?,
        )
        .await?;
    tx.commit().await?;
    Ok(())
}

pub(super) async fn exercise(first: &Store, second: &Store) -> Result<()> {
    let authority = Authority::new(first.clone(), DeploymentKey::fixture()?);
    let engine = Engine::join(authority.clone(), 30_000).await?;
    let pool = uuid::Uuid::new_v4().to_string();
    engine.set_resource(&pool, None, 2).await?;
    let a = Credentials::new(authority);
    let b = Credentials::new(Authority::new(second.clone(), DeploymentKey::fixture()?));
    let key = CredentialKey {
        provider: pool.clone(),
        account: "default".into(),
    };
    let stamp = a
        .import(&key, &pool, None, &Credential::Oauth(token("original", 1)))
        .await?;
    let refresh = Arc::new(Refresh {
        calls: AtomicUsize::new(0),
        entered: Notify::new(),
        release: Notify::new(),
    });
    let session_a = OAuthSession::new_inline(
        Arc::new(a.clone()),
        refresh.clone(),
        Duration::from_secs(30),
    );
    let session_b = OAuthSession::new_inline(
        Arc::new(b.clone()),
        refresh.clone(),
        Duration::from_secs(30),
    );
    for rejected in [None, Some("rotated-1")] {
        let session = session_a.clone();
        let slot = key.clone();
        let owned = tokio::spawn(async move {
            match rejected {
                None => session.resolve(&slot).await,
                Some(rejected) => session.recover(&slot, Some(rejected)).await,
            }
        });
        refresh.entered.notified().await;
        let account = first
            .get::<Value>(Kind::Credential, &stamp.id)
            .await?
            .context("credential disappeared")?;
        ensure!(
            account.value["lease"]["started"] == true,
            "refresh I/O preceded durable intent"
        );
        ensure!(
            session_b.resolve(&key).await.is_err(),
            "another worker reused an in-progress refresh token"
        );
        ensure!(b.load(&key).await.is_err());
        refresh.release.notify_one();
        let replaced = owned.await??;
        let fresh = session_b.recover(&key, Some("original")).await?;
        ensure!(fresh == replaced);
    }
    ensure!(refresh.calls.load(Ordering::SeqCst) == 2);
    let (current, _) = b.load(&key).await?;
    ensure!(current.version == 3);

    // Dropping an unused read lease is recoverable after expiry. Dropping a
    // lease after refresh intent is never permission to consume the token again.
    let unused = a.begin(&key).await?;
    drop(unused);
    expire(first, &stamp.id).await?;
    let mut reclaimed = b.begin(&key).await?;
    reclaimed.release().await?;
    let mut crashed = a.begin(&key).await?;
    crashed.prepare_refresh().await?;
    drop(crashed);
    expire(first, &stamp.id).await?;
    ensure!(
        b.begin(&key).await.is_err(),
        "expired refresh intent was replayable"
    );
    ensure!(session_b.resolve(&key).await.is_err());
    ensure!(refresh.calls.load(Ordering::SeqCst) == 2);
    let replacement = Credential::Oauth(token("operator-replacement", 0));
    let replaced = b
        .import(&key, &pool, Some(current.version), &replacement)
        .await?;
    ensure!(
        session_b.resolve(&key).await?
            == *replacement.as_oauth().context("fixture type mismatch")?
    );

    // Explicit operator replacement invalidates a still-running refresh owner.
    let mut stale = a.begin(&key).await?;
    stale.prepare_refresh().await?;
    let newer = b
        .import(
            &key,
            &pool,
            Some(replaced.version),
            &Credential::Oauth(token("newer-authority", 0)),
        )
        .await?;
    stale.stage(Credential::Oauth(token("stale-result", 0)));
    ensure!(stale.commit().await.is_err());
    ensure!(stale.release().await.is_err());
    let (actual, credential) = b.load(&key).await?;
    ensure!(actual == newer && credential == Credential::Oauth(token("newer-authority", 0)));
    let stored = first
        .get::<Value>(Kind::Credential, &stamp.id)
        .await?
        .context("credential disappeared")?;
    let encoded = serde_json::to_string(&stored.value)?;
    ensure!(!encoded.contains("newer-authority") && !encoded.contains("fixture-refresh"));

    // A lost refresh response has the same uncertainty as a killed owner.
    a.import(
        &key,
        &pool,
        Some(newer.version),
        &Credential::Oauth(token("expiring", 1)),
    )
    .await?;
    let failed = Arc::new(FailedRefresh(AtomicUsize::new(0)));
    let session = OAuthSession::new_inline(Arc::new(a), failed.clone(), Duration::from_secs(30));
    ensure!(session.resolve(&key).await.is_err());
    expire(first, &stamp.id).await?;
    ensure!(session.resolve(&key).await.is_err());
    ensure!(
        failed.0.load(Ordering::SeqCst) == 1,
        "network ambiguity replayed refresh I/O"
    );
    Ok(())
}
