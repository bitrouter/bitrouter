//! Cloud authentication glue and the `bitrouter cloud …` CLI entry points.
//!
//! Two daemon-side responsibilities, both keyed on the `"bitrouter"`
//! provider id:
//!
//! - [`enable_in_zero_config`] — auto-add the `bitrouter` provider to the in-memory
//!   zero-config `providers:` map when the user has signed in via
//!   `bitrouter cloud login` (an `account-credentials.json` file is present
//!   at the default path). The env-var path (`$BITROUTER_API_KEY`) is
//!   already covered by [`bitrouter_providers::zero_config`].
//! - [`register_if_configured`] — register the hosted provider applier.
//!
//! These are kept here rather than inside `bitrouter-providers` so that
//! the providers crate stays free of any dependency on
//! `bitrouter-cloud-sdk`; the SDK and the catalog can be consumed
//! independently by downstream tooling.
//!
//! The [`cli`] sub-module owns the `bitrouter cloud` subcommand surface
//! — typed wrappers around every endpoint on
//! [`bitrouter_cloud_sdk::management::ManagementClient`].

pub mod api;
pub mod cli;

use std::sync::Arc;

use anyhow::{Context, Result};
use bitrouter_observe::otel::TelemetryBearer;
use bitrouter_providers::hosted::account::credentials::default_credentials_path;
use bitrouter_providers::hosted::account::manager::CredentialManager;
use bitrouter_providers::hosted::applier::{BitrouterAuthApplier, PROVIDER_ID};
use bitrouter_sdk::config::{Config, ProviderConfig};
use bitrouter_sdk::language_model::auth::AuthAppliers;

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

/// Construct the default hosted account manager without reading its store.
pub fn default_manager() -> Result<Arc<CredentialManager>> {
    let path = default_credentials_path().context("resolving BitRouter Cloud credentials path")?;
    let manager = CredentialManager::new(path)
        .map_err(|error| anyhow::anyhow!("building BitRouter Cloud credential manager: {error}"))?;
    Ok(Arc::new(manager))
}

/// Register the BitRouter Cloud applier on `appliers` when the `bitrouter`
/// provider appears in `config.providers`. No-op otherwise.
pub fn register_if_configured(
    config: &Config,
    appliers: &mut AuthAppliers,
    manager: Arc<CredentialManager>,
) -> Result<()> {
    if !config.providers.contains_key(PROVIDER_ID) {
        return Ok(());
    }
    appliers.register(PROVIDER_ID, Arc::new(BitrouterAuthApplier::new(manager)));
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
    manager: Arc<CredentialManager>,
    expected_origin: String,
}

impl std::fmt::Debug for CloudBearer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("CloudBearer")
            .field("manager", &"<redacted>")
            .field("expected_origin", &self.expected_origin)
            .finish()
    }
}

#[async_trait::async_trait]
impl TelemetryBearer for CloudBearer {
    async fn bearer(&self) -> Option<String> {
        self.manager
            .resolve_bearer(None, Some(&self.expected_origin))
            .await
            .map(|credential| credential.secret().to_owned())
            .ok()
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
pub async fn cloud_bearer_source(
    manager: Arc<CredentialManager>,
    expected_origin: impl Into<String>,
) -> Option<Arc<dyn TelemetryBearer>> {
    manager.current().await.ok()??;
    Some(Arc::new(CloudBearer {
        manager,
        expected_origin: expected_origin.into(),
    }))
}

/// Resolve the signed-in Cloud bearer only when `target_base_url` has the
/// exact origin recorded at login. This permits headless gateway clients to
/// reuse `bitrouter cloud login` without ever forwarding that credential to
/// an arbitrary remote host.
pub async fn cloud_bearer_for_base_url(target_base_url: &str) -> Option<String> {
    let manager = default_manager().ok()?;
    cloud_bearer_for_base_url_with_manager(manager, target_base_url).await
}

/// Resolve a hosted bearer for `target_base_url` using the supplied manager.
pub async fn cloud_bearer_for_base_url_with_manager(
    manager: Arc<CredentialManager>,
    target_base_url: &str,
) -> Option<String> {
    manager
        .resolve_bearer(None, Some(target_base_url))
        .await
        .map(|credential| credential.secret().to_owned())
        .ok()
}

/// Resolve only a static inference API key for an exact Cloud origin. OAuth
/// access tokens use `Authorization: Bearer`; settlement receipts are scoped
/// by `x-api-key`, so silently coercing OAuth into that header is invalid.
pub async fn cloud_api_key_for_base_url(target_base_url: &str) -> Option<String> {
    let manager = default_manager().ok()?;
    manager
        .resolve_api_key(None, Some(target_base_url))
        .await
        .map(|credential| credential.secret().to_owned())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_providers::hosted::account::credentials::{Credentials, StoredCredential};
    use bitrouter_providers::hosted::account::manager::CredentialManager;
    use bitrouter_providers::hosted::applier::BitrouterAuthApplier;
    use bitrouter_sdk::language_model::AuthApplier;
    use bitrouter_sdk::language_model::types::{ApiProtocol, RoutingTarget};
    use chrono::{Duration, Utc};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fresh_tmp_creds_path(label: &str) -> anyhow::Result<std::path::PathBuf> {
        let directory = tempfile::Builder::new()
            .prefix(&format!("bitrouter-cloud-glue-{label}-"))
            .tempdir()?;
        Ok(directory.keep().join("account-credentials.json"))
    }

    fn target_for_origin(origin: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: bitrouter_providers::hosted::applier::PROVIDER_ID.to_owned(),
            service_id: "gpt-4o".to_owned(),
            api_base: origin.to_owned(),
            api_key: String::new(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    #[tokio::test]
    async fn shared_manager_single_flights_oauth_refresh_for_auth_and_telemetry()
    -> anyhow::Result<()> {
        let server = MockServer::start().await;
        let origin = server.uri();
        Mock::given(method("GET"))
            .and(wm_path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "issuer": origin,
                "device_authorization_endpoint": format!("{origin}/oauth/device_authorization"),
                "token_endpoint": format!("{origin}/oauth/token"),
            })))
            .mount(&server)
            .await;
        Mock::given(method("POST"))
            .and(wm_path("/oauth/token"))
            .and(body_string_contains("grant_type=refresh_token"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "access_token": "rotated-access",
                "token_type": "Bearer",
                "expires_in": 3600,
                "refresh_token": "rotated-refresh",
                "scope": "inference:invoke",
            })))
            .mount(&server)
            .await;

        let path = fresh_tmp_creds_path("shared-refresh")?;
        let manager = Arc::new(CredentialManager::with_client(
            path.clone(),
            reqwest::Client::new(),
        ));
        manager
            .save(
                bitrouter_providers::hosted::account::credentials::StoredCredential::from(
                    bitrouter_providers::hosted::account::credentials::Credentials {
                        access_token: "stale-access".to_owned(),
                        refresh_token: Some("original-refresh".to_owned()),
                        expires_at: Utc::now() + Duration::seconds(10),
                        refresh_token_expires_at: None,
                        token_type: "Bearer".to_owned(),
                        scope: "inference:invoke".to_owned(),
                        client_id: "bitrouter-cli".to_owned(),
                        authorization_server: origin.clone(),
                        namespace_id: Some("ns-test".to_owned()),
                        subject: None,
                    },
                ),
            )
            .await?;

        let applier = BitrouterAuthApplier::new(Arc::clone(&manager));
        let telemetry = CloudBearer {
            manager: Arc::clone(&manager),
            expected_origin: origin.clone(),
        };
        let request = reqwest::Client::new().post(&origin).build()?;
        let target = target_for_origin(&origin);
        let (applied, bearer) = tokio::join!(applier.apply(request, &target), telemetry.bearer(),);
        assert_eq!(
            applied?.headers()[reqwest::header::AUTHORIZATION],
            "Bearer rotated-access"
        );
        assert_eq!(bearer.as_deref(), Some("rotated-access"));
        let current = manager
            .current()
            .await?
            .ok_or_else(|| anyhow::anyhow!("rotated credential was not persisted"))?;
        let oauth = current
            .oauth()
            .ok_or_else(|| anyhow::anyhow!("rotated credential is not OAuth"))?;
        assert_eq!(oauth.refresh_token.as_deref(), Some("rotated-refresh"));
        let refreshes = server
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("wiremock did not record requests"))?
            .into_iter()
            .filter(|request| request.method.to_string() == "POST")
            .count();
        assert_eq!(refreshes, 1);
        Ok(())
    }

    #[test]
    fn enable_in_zero_config_noop_when_no_credentials_file() -> anyhow::Result<()> {
        let path = fresh_tmp_creds_path("noop")?;
        // path's parent exists; the file itself does not.
        let mut config = Config::default();
        enable_in_zero_config_with_path(&mut config, &path);
        assert!(!config.providers.contains_key(PROVIDER_ID));
        Ok(())
    }

    #[test]
    fn enable_in_zero_config_inserts_when_credentials_file_present() -> anyhow::Result<()> {
        let path = fresh_tmp_creds_path("inserts")?;
        std::fs::write(&path, "{}")?;
        let mut config = Config::default();
        enable_in_zero_config_with_path(&mut config, &path);
        let provider = match config.providers.get(PROVIDER_ID) {
            Some(provider) => provider,
            None => anyhow::bail!("bitrouter provider was not auto-enabled"),
        };
        assert!(
            provider.auto_discover,
            "auto_discover should be true so /models populates the routable list"
        );
        Ok(())
    }

    #[test]
    fn enable_in_zero_config_noop_when_already_configured() -> anyhow::Result<()> {
        let path = fresh_tmp_creds_path("already")?;
        std::fs::write(&path, "{}")?;
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
        let provider = match config.providers.get(PROVIDER_ID) {
            Some(provider) => provider,
            None => anyhow::bail!("configured bitrouter provider disappeared"),
        };
        assert_eq!(provider.api_base, "https://example.invalid");
        Ok(())
    }

    #[tokio::test]
    async fn cloud_bearer_source_none_when_not_signed_in() -> anyhow::Result<()> {
        let manager = Arc::new(CredentialManager::with_client(
            fresh_tmp_creds_path("absent")?,
            reqwest::Client::new(),
        ));
        assert!(
            cloud_bearer_source(manager, "https://telemetry.bitrouter.ai")
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn cloud_bearer_source_uses_same_origin_api_key() -> anyhow::Result<()> {
        let manager = Arc::new(CredentialManager::with_client(
            fresh_tmp_creds_path("api-key-bearer")?,
            reqwest::Client::new(),
        ));
        manager
            .save(StoredCredential::api_key(
                "brk_telemetry.secret".to_owned(),
                "https://telemetry.bitrouter.ai".to_owned(),
            ))
            .await?;
        let source = match cloud_bearer_source(manager, "https://telemetry.bitrouter.ai").await {
            Some(source) => source,
            None => anyhow::bail!("stored API key did not produce a telemetry source"),
        };
        assert_eq!(
            source.bearer().await.as_deref(),
            Some("brk_telemetry.secret")
        );
        Ok(())
    }

    #[tokio::test]
    async fn cloud_gateway_bearer_is_scoped_to_the_login_origin() -> anyhow::Result<()> {
        let manager = Arc::new(CredentialManager::with_client(
            fresh_tmp_creds_path("gateway-origin")?,
            reqwest::Client::new(),
        ));
        manager
            .save(StoredCredential::api_key(
                "brk_gateway.secret".to_owned(),
                "https://api.bitrouter.ai".to_owned(),
            ))
            .await?;

        assert_eq!(
            cloud_bearer_for_base_url_with_manager(
                Arc::clone(&manager),
                "https://api.bitrouter.ai/v1/responses",
            )
            .await
            .as_deref(),
            Some("brk_gateway.secret")
        );
        assert!(
            cloud_bearer_for_base_url_with_manager(manager, "https://api.bitrouter.ai.evil/v1")
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn settlement_resolver_rejects_oauth_but_accepts_same_origin_api_key()
    -> anyhow::Result<()> {
        let oauth = Arc::new(CredentialManager::with_client(
            fresh_tmp_creds_path("settlement-oauth")?,
            reqwest::Client::new(),
        ));
        oauth
            .save(StoredCredential::from(Credentials {
                access_token: "oauth-access-token".to_owned(),
                refresh_token: Some("oauth-refresh-token".to_owned()),
                expires_at: Utc::now() + Duration::minutes(10),
                refresh_token_expires_at: None,
                token_type: "Bearer".to_owned(),
                scope: "inference:invoke".to_owned(),
                client_id: "bitrouter-cli".to_owned(),
                authorization_server: "https://api.bitrouter.ai".to_owned(),
                namespace_id: Some("ns-test".to_owned()),
                subject: None,
            }))
            .await?;
        assert!(
            oauth
                .resolve_api_key(None, Some("https://api.bitrouter.ai"))
                .await
                .is_err()
        );

        let api_key = Arc::new(CredentialManager::with_client(
            fresh_tmp_creds_path("settlement-api-key")?,
            reqwest::Client::new(),
        ));
        api_key
            .save(StoredCredential::api_key(
                "brk_gateway.secret".to_owned(),
                "https://api.bitrouter.ai".to_owned(),
            ))
            .await?;
        let resolved = api_key
            .resolve_api_key(None, Some("https://api.bitrouter.ai/v1"))
            .await?;
        assert_eq!(resolved.secret(), "brk_gateway.secret");
        Ok(())
    }

    #[test]
    fn cloud_bearer_debug_redacts_manager() -> anyhow::Result<()> {
        let manager = Arc::new(CredentialManager::with_client(
            fresh_tmp_creds_path("dbg")?,
            reqwest::Client::new(),
        ));
        let bearer = CloudBearer {
            manager,
            expected_origin: "https://telemetry.bitrouter.ai".to_owned(),
        };
        let rendered = format!("{bearer:?}");
        assert!(rendered.contains("<redacted>"));
        Ok(())
    }

    #[tokio::test]
    async fn malformed_credentials_file_is_swallowed_as_anonymous() -> anyhow::Result<()> {
        let path = fresh_tmp_creds_path("malformed")?;
        std::fs::write(&path, "{ not valid json")?;
        let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
        assert!(
            cloud_bearer_source(manager, "https://telemetry.bitrouter.ai")
                .await
                .is_none()
        );
        Ok(())
    }
}
