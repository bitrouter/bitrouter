#![cfg(feature = "file-store")]

use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use bitrouter_ai::auth::credentials::{Credential, OAuthToken};
use bitrouter_ai::auth::file::backend::FileCredentialStore;
use bitrouter_ai::auth::file::snapshot::CredentialStore as Snapshot;
use bitrouter_ai::auth::store::{
    CredentialKey, CredentialStore, Durability, OAuthRefresher, OAuthSession, StoreError,
};
use tokio::sync::Notify;

type TestResult = Result<(), Box<dyn std::error::Error + Send + Sync>>;

fn key() -> CredentialKey {
    CredentialKey {
        provider: "selected".into(),
        account: "private-account".into(),
    }
}

fn token(value: &str, expires_at: u64) -> Credential {
    Credential::Oauth(OAuthToken {
        access_token: value.into(),
        expires_at,
        refresh_token: Some(format!("refresh-{value}")),
    })
}

#[test]
fn legacy_read_is_nonmutating_and_writes_preserve_unrelated_slots() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("caller-path-secret.json");
    let legacy = br#"{"selected":{"access_token":"legacy-secret","expires_at":0,"refresh_token":"refresh-secret"}}"#;
    std::fs::write(&path, legacy)?;
    let mut first = Snapshot::load(&path)?;
    assert_eq!(std::fs::read(&path)?, legacy);
    assert!(first.get_any("selected", "default").is_some());
    let mut stale = Snapshot::load(&path)?;
    first.set("selected", "private-account", token("one-secret", 0))?;
    stale.set(
        "elsewhere",
        "other-account",
        Credential::api_key("two-secret"),
    )?;
    let debug = format!("{:?}", Snapshot::load(&path)?);
    for secret in [
        "legacy-secret",
        "refresh-secret",
        "one-secret",
        "two-secret",
        "private-account",
        "caller-path-secret",
    ] {
        assert!(!debug.contains(secret));
    }
    let final_store = Snapshot::load(&path)?;
    assert!(final_store.get_any("selected", "default").is_some());
    assert_eq!(
        final_store.get_any("selected", "private-account"),
        Some(&token("one-secret", 0))
    );
    assert_eq!(
        final_store
            .get_any("elsewhere", "other-account")
            .and_then(Credential::as_api_key),
        Some("two-secret")
    );
    Ok(())
}

#[tokio::test]
async fn staged_replacement_survives_failed_write_and_shared_lease_release() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("selected.json");
    Snapshot::load(&path)?.set("selected", "private-account", token("old", 0))?;
    let first = FileCredentialStore::new(&path)?;
    let second = FileCredentialStore::new(dir.path().join(".").join("selected.json"))?;
    let mut lease = first.begin(&key()).await?;
    lease.stage(token("rotated", 0));
    let blocked = path.with_extension("json.tmp");
    std::fs::create_dir(&blocked)?;
    assert_eq!(lease.commit().await, Err(StoreError::Unavailable));
    let selected_key = key();
    let next = second.begin(&selected_key);
    tokio::pin!(next);
    assert!(futures::poll!(next.as_mut()).is_pending());
    drop(lease);
    std::fs::remove_dir(&blocked)?;
    let mut next = next.await?;
    assert_eq!(next.pending(), Some(&token("rotated", 0)));
    assert_eq!(next.commit().await?, Durability::Persistent);
    assert_eq!(
        Snapshot::load(&path)?.get_any("selected", "private-account"),
        Some(&token("rotated", 0))
    );
    Ok(())
}

struct GateRefresh {
    entered: Notify,
    release: Notify,
    calls: AtomicUsize,
}

#[async_trait::async_trait]
impl OAuthRefresher for GateRefresh {
    async fn refresh(&self, _current: &OAuthToken) -> bitrouter_ai::error::Result<OAuthToken> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        self.entered.notify_one();
        self.release.notified().await;
        Ok(OAuthToken {
            access_token: "rotated".into(),
            expires_at: 0,
            refresh_token: Some("new-refresh".into()),
        })
    }
}

#[tokio::test]
async fn cancelling_a_caller_preserves_owned_refresh_and_file_commit() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("selected.json");
    Snapshot::load(&path)?.set("selected", "private-account", token("expired", 1))?;
    let refresh = Arc::new(GateRefresh {
        entered: Notify::new(),
        release: Notify::new(),
        calls: AtomicUsize::new(0),
    });
    let session = OAuthSession::new(
        Arc::new(FileCredentialStore::new(&path)?),
        refresh.clone(),
        Duration::from_secs(60),
    );
    let caller = tokio::spawn(async move { session.resolve(&key()).await });
    tokio::time::timeout(Duration::from_secs(2), refresh.entered.notified()).await?;
    caller.abort();
    assert!(
        caller
            .await
            .err()
            .ok_or("cancelled caller returned")?
            .is_cancelled()
    );
    refresh.release.notify_one();
    let other = FileCredentialStore::new(&path)?;
    let selected_key = key();
    let lease = tokio::time::timeout(Duration::from_secs(2), other.begin(&selected_key)).await??;
    assert_eq!(
        lease
            .credential()
            .and_then(Credential::as_oauth)
            .map(|token| token.access_token.as_str()),
        Some("rotated")
    );
    drop(lease);
    let fresh = OAuthSession::new(Arc::new(other), refresh.clone(), Duration::from_secs(60))
        .resolve(&selected_key)
        .await?;
    assert_eq!(fresh.refresh_token.as_deref(), Some("new-refresh"));
    assert_eq!(refresh.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn administrative_logout_through_a_file_alias_blocks_stale_refresh() -> TestResult {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("selected.json");
    let alias = dir.path().join("alias.json");
    Snapshot::load(&path)?.set("selected", "private-account", token("old", 0))?;
    std::os::unix::fs::symlink(&path, &alias)?;
    let backend = FileCredentialStore::new(&path)?;
    let mut lease = backend.begin(&key()).await?;
    lease.stage(token("stale-refresh", 0));
    Snapshot::load(&alias)?.remove("selected", "private-account")?;
    assert_eq!(lease.commit().await, Err(StoreError::Conflict));
    assert!(std::fs::symlink_metadata(&alias)?.file_type().is_symlink());
    assert!(
        Snapshot::load(&path)?
            .get_any("selected", "private-account")
            .is_none()
    );
    Ok(())
}

#[tokio::test]
async fn backend_created_before_missing_parents_still_shares_its_account_gate() -> TestResult {
    let dir = tempfile::tempdir()?;
    let canonical = std::fs::canonicalize(dir.path())?;
    let path = dir.path().join("new-parent").join("selected.json");
    let first = FileCredentialStore::new(&path)?;
    Snapshot::load(&path)?.set("selected", "private-account", token("old", 0))?;
    let second = FileCredentialStore::new(canonical.join("new-parent").join("selected.json"))?;
    let lease = first.begin(&key()).await?;
    let selected_key = key();
    let next = second.begin(&selected_key);
    tokio::pin!(next);
    assert!(futures::poll!(next.as_mut()).is_pending());
    drop(lease);
    assert!(next.await?.credential().is_some());
    Ok(())
}

#[test]
fn relative_store_binding_survives_a_later_working_directory_change() -> TestResult {
    let dir = tempfile::tempdir()?;
    let output = std::process::Command::new(std::env::current_exe()?)
        .args(["--exact", "relative_store_child", "--nocapture"])
        .env("BITROUTER_AI_FILE_STORE_CHILD", "1")
        .current_dir(dir.path())
        .output()?;
    assert!(
        output.status.success(),
        "{}",
        String::from_utf8_lossy(&output.stderr)
    );
    assert!(dir.path().join("caller-selected.json").exists());
    assert!(
        !dir.path()
            .join("changed")
            .join("caller-selected.json")
            .exists()
    );
    Ok(())
}

#[test]
fn relative_store_child() -> TestResult {
    if std::env::var_os("BITROUTER_AI_FILE_STORE_CHILD").is_none() {
        return Ok(());
    }
    let mut store = Snapshot::load("caller-selected.json")?;
    std::fs::create_dir("changed")?;
    std::env::set_current_dir("changed")?;
    store.set("selected", "private-account", Credential::api_key("value"))?;
    Ok(())
}
