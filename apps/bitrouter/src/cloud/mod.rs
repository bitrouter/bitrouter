//! Glue between [`bitrouter_cloud_sdk`] and the bitrouter assembly
//! layer, plus the `bitrouter cloud …` CLI entry points (see [`cli`]).
//!
//! Two daemon-side responsibilities, both keyed on the `"bitrouter"`
//! provider id:
//!
//! - [`enable_in_zero_config`] — auto-add the `bitrouter` provider to the in-memory
//!   zero-config `providers:` map when the user has signed in via
//!   `bitrouter cloud login` (an `account-credentials.json` file is present
//!   at the default path). The env-var path (`$BITROUTER_API_KEY`) is
//!   already covered by [`bitrouter_providers::zero_config`].
//! - [`build_auth_applier`] — construct the
//!   [`bitrouter_cloud_sdk::BitrouterCloudAuthApplier`] for registration
//!   against the SDK executor.
//!
//! These are kept here rather than inside `bitrouter-providers` so that
//! the providers crate stays free of any dependency on
//! `bitrouter-cloud-sdk`; the SDK and the catalog can be consumed
//! independently by downstream tooling.
//!
//! The [`cli`] sub-module owns the `bitrouter cloud` subcommand surface
//! — typed wrappers around every endpoint on
//! [`bitrouter_cloud_sdk::management::ManagementClient`].

pub mod cli;

use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_cloud_sdk::BitrouterCloudAuthApplier;
use bitrouter_cloud_sdk::auth::credentials::{CredentialsStore, default_credentials_path};
use bitrouter_cloud_sdk::auth::metadata::AsMetadata;
use bitrouter_cloud_sdk::provider::PROVIDER_ID;
use bitrouter_observe::otel::TelemetryBearer;
use bitrouter_sdk::config::{Config, ProviderConfig};
use bitrouter_sdk::language_model::{AuthApplier, auth::AuthAppliers};

/// Insert the `bitrouter` provider into `config.providers` when the user
/// has run `bitrouter cloud login` (i.e. the credentials file exists at the
/// default path) and the entry is not already present.
///
/// No-op when the credentials file is absent — `bitrouter_providers::zero_config`
/// already handles the `$BITROUTER_API_KEY` env-var path. Together the two
/// paths give a signed-in user the cloud provider on every fresh
/// `bitrouter serve` regardless of which credential source they chose.
pub fn enable_in_zero_config(config: &mut Config) {
    let Ok(path) = default_credentials_path() else {
        return;
    };
    enable_in_zero_config_with_path(config, &path);
}

/// Inner form taking the credentials path explicitly so unit tests can
/// drive the logic without mutating process environment.
fn enable_in_zero_config_with_path(config: &mut Config, credentials_path: &std::path::Path) {
    if config.providers.contains_key(PROVIDER_ID) {
        return;
    }
    if !credentials_path.exists() {
        return;
    }
    config.providers.insert(
        PROVIDER_ID.to_string(),
        ProviderConfig {
            auto_discover: true,
            ..ProviderConfig::default()
        },
    );
}

/// Build the BitRouter Cloud `AuthApplier`. Reads the credentials file at
/// [`bitrouter_cloud_sdk::auth::credentials::default_credentials_path`].
pub fn build_auth_applier() -> Result<Arc<dyn AuthApplier>> {
    let path = default_credentials_path().context("resolving BitRouter Cloud credentials path")?;
    let applier =
        BitrouterCloudAuthApplier::new(path).context("building the BitRouter Cloud AuthApplier")?;
    Ok(Arc::new(applier))
}

/// Register the BitRouter Cloud applier on `appliers` when the `bitrouter`
/// provider appears in `config.providers`. No-op otherwise.
pub fn register_if_configured(config: &Config, appliers: &mut AuthAppliers) -> Result<()> {
    if !config.providers.contains_key(PROVIDER_ID) {
        return Ok(());
    }
    let applier = build_auth_applier()?;
    appliers.register(PROVIDER_ID, applier);
    Ok(())
}

/// Live [`TelemetryBearer`] backed by the signed-in account's credential store.
///
/// Resolves the account bearer **on every OTLP export** (not once at startup),
/// transparently refreshing the short-lived access token via the store's
/// [`CredentialsStore::current_token`] — which refreshes-if-near-expiry,
/// single-flights, and writes the rotated token back to disk. This is what keeps
/// account-attributed telemetry alive across token expiry without a daemon
/// restart, replacing the old startup-snapshot baked into a static header.
///
/// Best-effort: any resolution failure maps to `None`, so the export degrades to
/// anonymous rather than being dropped.
pub struct CloudBearer {
    /// The store is mutated by `current_token` (it writes the refreshed token
    /// back), so it sits behind an async mutex shared across concurrent exports.
    /// In practice the OTLP batch processor exports serially, but the mutex makes
    /// the refresh-and-persist single-flight correct regardless.
    store: tokio::sync::Mutex<CredentialsStore>,
    /// Client used for the RFC 6749 §6 refresh exchange.
    client: reqwest::Client,
    /// Cached AS metadata (token endpoint, etc.), fetched once at construction.
    metadata: AsMetadata,
}

impl std::fmt::Debug for CloudBearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Never render the store (it holds the bearer / refresh token) — redact.
        f.debug_struct("CloudBearer")
            .field("store", &"<redacted>")
            .field("metadata", &self.metadata)
            .finish()
    }
}

#[async_trait::async_trait]
impl TelemetryBearer for CloudBearer {
    async fn bearer(&self) -> Option<String> {
        // `current_token` refreshes-if-near-expiry, single-flights, and persists
        // the rotated token. Any error (no stored creds, refresh failure, …) is
        // swallowed to `None` so the export stays anonymous — best-effort.
        let mut store = self.store.lock().await;
        store.current_token(&self.client, &self.metadata).await.ok()
    }
}

/// Build a live telemetry-bearer source from the signed-in account, or `None`
/// when not signed in (or the AS metadata can't be fetched).
///
/// Best-effort: every failure (no credential store, no current credential,
/// metadata fetch failure) yields `None` so telemetry exports anonymously and
/// daemon startup is never broken. The caller decides whether to build a source
/// at all — `attribution: anonymous` must never call this (it would read the
/// credential store).
pub async fn cloud_bearer_source() -> Option<Arc<dyn TelemetryBearer>> {
    let store = CredentialsStore::default_path().ok()?;
    cloud_bearer_source_from_store(store).await
}

/// Inner form of [`cloud_bearer_source`] taking an already-loaded store, so the
/// "not signed in ⇒ None" decision is testable without the default path or a
/// live AS-metadata fetch. Requires a current credential (else `None`), then
/// fetches the AS metadata its `authorization_server` advertises.
async fn cloud_bearer_source_from_store(
    store: CredentialsStore,
) -> Option<Arc<dyn TelemetryBearer>> {
    let creds = store.current()?;
    let client = reqwest::Client::new();
    let metadata = bitrouter_cloud_sdk::auth::metadata::fetch(&client, &creds.authorization_server)
        .await
        .ok()?;
    Some(Arc::new(CloudBearer {
        store: tokio::sync::Mutex::new(store),
        client,
        metadata,
    }))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::{AtomicU64, Ordering};

    fn fresh_tmp_creds_path(label: &str) -> std::path::PathBuf {
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-cloud-glue-{label}-{}-{id}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir.join("account-credentials.json")
    }

    #[test]
    fn enable_in_zero_config_noop_when_no_credentials_file() {
        let path = fresh_tmp_creds_path("noop");
        // path's parent exists; the file itself does not.
        let mut config = Config::default();
        enable_in_zero_config_with_path(&mut config, &path);
        assert!(!config.providers.contains_key(PROVIDER_ID));
    }

    #[test]
    fn enable_in_zero_config_inserts_when_credentials_file_present() {
        let path = fresh_tmp_creds_path("inserts");
        fs::write(&path, "{}").unwrap();
        let mut config = Config::default();
        enable_in_zero_config_with_path(&mut config, &path);
        let provider = config
            .providers
            .get(PROVIDER_ID)
            .expect("`bitrouter` provider should be auto-enabled when creds file is present");
        assert!(
            provider.auto_discover,
            "auto_discover should be true so /models populates the routable list"
        );
    }

    #[test]
    fn enable_in_zero_config_noop_when_already_configured() {
        let path = fresh_tmp_creds_path("already");
        fs::write(&path, "{}").unwrap();
        let mut config = Config::default();
        // Pre-populate with a sentinel `api_base` so we can prove we didn't
        // overwrite the existing entry.
        config.providers.insert(
            PROVIDER_ID.to_string(),
            ProviderConfig {
                api_base: "https://example.invalid".to_string(),
                ..ProviderConfig::default()
            },
        );
        enable_in_zero_config_with_path(&mut config, &path);
        assert_eq!(
            config.providers.get(PROVIDER_ID).unwrap().api_base,
            "https://example.invalid",
            "existing entry must not be overwritten"
        );
    }

    /// Write a credentials JSON file with the given access token and RFC 3339
    /// `expires_at`, then load it into a store. The remaining required fields
    /// are filled with placeholders — only the token + expiry matter here.
    fn store_with_token(label: &str, access_token: &str, expires_at: &str) -> CredentialsStore {
        let path = fresh_tmp_creds_path(label);
        let json = serde_json::json!({
            "access_token": access_token,
            "expires_at": expires_at,
            "token_type": "Bearer",
            "scope": "telemetry:write",
            "client_id": "bitrouter-cli",
            "authorization_server": "https://api.bitrouter.ai",
        });
        fs::write(&path, serde_json::to_vec(&json).unwrap()).unwrap();
        CredentialsStore::load(&path).unwrap()
    }

    #[tokio::test]
    async fn cloud_bearer_source_none_when_not_signed_in() {
        // Empty store (no credentials file) → no live source, and crucially no
        // AS-metadata network call (the `store.current()?` short-circuits first).
        let path = fresh_tmp_creds_path("absent");
        let store = CredentialsStore::load(&path).unwrap();
        assert!(store.current().is_none(), "precondition: empty store");
        assert!(
            cloud_bearer_source_from_store(store).await.is_none(),
            "an empty credential store must not build a live bearer source"
        );
    }

    #[test]
    fn cloud_bearer_debug_redacts_store() {
        // The `Debug` impl must never render the store (it holds the bearer /
        // refresh token). A signed-in store would be `Some(creds)`; just prove
        // the field is redacted on a constructed value.
        let store = store_with_token("dbg", "bra_secret", "2999-01-01T00:00:00Z");
        let bearer = CloudBearer {
            store: tokio::sync::Mutex::new(store),
            client: reqwest::Client::new(),
            metadata: AsMetadata {
                issuer: Some("https://api.bitrouter.ai".to_string()),
                device_authorization_endpoint: "https://api.bitrouter.ai/device".to_string(),
                token_endpoint: "https://api.bitrouter.ai/token".to_string(),
                revocation_endpoint: None,
            },
        };
        let rendered = format!("{bearer:?}");
        assert!(
            !rendered.contains("bra_secret"),
            "bearer token leaked in Debug: {rendered}"
        );
        assert!(rendered.contains("<redacted>"));
    }

    #[test]
    fn malformed_credentials_file_is_swallowed_as_anonymous() {
        // A corrupt credentials file makes `CredentialsStore::load` error; the
        // `default_path().ok()?` in `cloud_bearer_source` swallows that into
        // `None` so a broken file degrades to anonymous telemetry rather than
        // breaking daemon startup. We can't drive the real default path here, so
        // assert the load error that the `?` consumes.
        let path = fresh_tmp_creds_path("malformed");
        fs::write(&path, "{ not valid json").unwrap();
        assert!(
            CredentialsStore::load(&path).is_err(),
            "a malformed credentials file must surface as a load error for \
             `cloud_bearer_source` to swallow into anonymous"
        );
    }
}
