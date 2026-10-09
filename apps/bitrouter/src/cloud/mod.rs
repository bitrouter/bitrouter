//! Cloud authentication glue and the `bro cloud …` CLI entry points.
//!
//! Two daemon-side responsibilities, both keyed on the `"bitrouter"`
//! provider id:
//!
//! - [`enable_in_zero_config`] — auto-add the `bitrouter` provider to the in-memory
//!   zero-config `providers:` map when the user has signed in via
//!   `bro cloud login` (an `account-credentials.json` file is present
//!   at the default path). The env-var path (`$BITROUTER_API_KEY`) is
//!   already covered by [`crate::providers::apply::zero_config`].
//! - [`register_if_configured`] — register the hosted provider applier.
//!
//! The account module owns product persistence/settings and assembles the AI
//! hosted session shared by model, Cloud management and telemetry consumers.
//!
//! The [`cli`] sub-module owns the `bro cloud` subcommand surface
//! — typed wrappers around the application-owned management client.

pub mod account;
pub mod api;
pub mod api_client;
pub mod auth;
pub mod cli;
pub mod management;
pub mod settlement;

use std::sync::Arc;

use crate::cloud::account::credentials::default_credentials_path;
use crate::cloud::account::manager::CredentialManager;
use anyhow::{Context, Result};
use bitrouter_ai::auth::AuthAppliers;
use bitrouter_ai::providers::hosted::PROVIDER_ID;
use bitrouter_ai::providers::hosted::applier::BitrouterAuthApplier;
use bitrouter_sdk::config::{Config, ProviderConfig};
use bitrouter_telemetry::otel::TelemetryBearer;

const FIRST_PARTY_TELEMETRY_ORIGIN: &str = "https://telemetry.bitrouter.ai";
const DEFAULT_ACCOUNT_ORIGIN: &str = "https://api.bitrouter.ai";

/// Insert the `bitrouter` provider into `config.providers` when the user
/// has run `bro cloud login` (i.e. the credentials file exists at the
/// default path) and the entry is not already present.
///
/// No-op when the credentials file is absent — `crate::providers::apply::zero_config`
/// already handles the `$BITROUTER_API_KEY` env-var path. Together the two
/// paths give a signed-in user the cloud provider on every fresh
/// `bro serve` regardless of which credential source they chose.
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

/// One lazily constructed account manager shared by the Cloud consumers of a
/// standalone ACP invocation. Construction does not read the credential store;
/// resolution remains best-effort at each consumer boundary.
#[derive(Clone)]
pub(crate) struct StandaloneCloudCredentials {
    manager: Option<Arc<CredentialManager>>,
}

impl StandaloneCloudCredentials {
    /// Construct the invocation-scoped manager once. A path-construction
    /// failure preserves the existing best-effort fallback behavior.
    pub(crate) fn new() -> Self {
        Self {
            manager: default_manager().ok(),
        }
    }

    #[cfg(test)]
    pub(crate) fn with_manager(manager: Arc<CredentialManager>) -> Self {
        Self {
            manager: Some(manager),
        }
    }

    /// Resolve the stored account bearer for ACP routing, when available.
    pub(crate) async fn routing_fallback(&self, target_base_url: &str) -> Option<String> {
        let manager = self.manager.as_ref()?.clone();
        cloud_bearer_for_base_url_with_manager(manager, target_base_url).await
    }

    /// Build the account telemetry source from the same invocation manager.
    pub(crate) async fn telemetry_bearer(
        &self,
        endpoint: &str,
    ) -> Option<Arc<dyn TelemetryBearer>> {
        let manager = self.manager.as_ref()?.clone();
        cloud_bearer_source(manager, endpoint).await
    }
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
    appliers.register(
        PROVIDER_ID,
        Arc::new(BitrouterAuthApplier::new(
            manager.session().clone(),
            onboarding_hint(),
        )),
    );
    Ok(())
}

/// Live [`TelemetryBearer`] backed by the signed-in account manager.
///
/// Resolves the account bearer **on every OTLP export** (not once at startup),
/// transparently refreshing the short-lived access token through the manager,
/// which refreshes-if-near-expiry, single-flights, and writes the rotated token
/// back to disk. This is what keeps
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
            .session()
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
    let expected_origin = credential_origin_for_exporter(&expected_origin.into());
    Some(Arc::new(CloudBearer {
        manager,
        expected_origin,
    }))
}

/// Return the credential origin permitted for an OTLP exporter.
///
/// Account credentials are normally confined to the exporter's origin. The
/// sole exception is the first-party telemetry service, which accepts the
/// account credential registered at the hosted inference origin.
fn credential_origin_for_exporter(exporter: &str) -> String {
    let exporter_origin = reqwest::Url::parse(exporter)
        .ok()
        .map(|url| url.origin().ascii_serialization());
    match exporter_origin.as_deref() {
        Some(FIRST_PARTY_TELEMETRY_ORIGIN) => DEFAULT_ACCOUNT_ORIGIN.to_owned(),
        _ => exporter.to_owned(),
    }
}

/// Resolve the signed-in Cloud bearer only when `target_base_url` has the
/// exact origin recorded at login. This permits headless gateway clients to
/// reuse `bro cloud login` without ever forwarding that credential to
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
        .session()
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
        .session()
        .resolve_api_key(None, Some(target_base_url))
        .await
        .map(|credential| credential.secret().to_owned())
        .ok()
}

fn onboarding_hint() -> String {
    format!(
        "no BitRouter Cloud credential — run `{} cloud login` or set BITROUTER_API_KEY=brk_…",
        bitrouter_sdk::invocation::name()
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::cloud::account::manager::CredentialManager;
    use bitrouter_ai::auth::AuthApplier;
    use bitrouter_ai::providers::hosted::credentials::{Credentials, StoredCredential};
    use bitrouter_ai::types::ApiProtocol;
    use bitrouter_sdk::model_call::types::RoutingTarget;
    use chrono::{Duration, Utc};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    fn fresh_tmp_creds_path(
        label: &str,
    ) -> anyhow::Result<(tempfile::TempDir, std::path::PathBuf)> {
        let directory = tempfile::Builder::new()
            .prefix(&format!("bitrouter-cloud-glue-{label}-"))
            .tempdir()?;
        let path = directory.path().join("account-credentials.json");
        Ok((directory, path))
    }

    fn target_for_origin(origin: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: bitrouter_ai::providers::hosted::PROVIDER_ID.to_owned(),
            service_id: "gpt-4o".to_owned(),
            api_base: origin.to_owned(),
            api_key: String::new(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            chat_google_extensions: false,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
        }
    }

    #[tokio::test]
    async fn configured_inference_key_bypasses_saved_cloud_credentials() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        for credential in [
            Some(StoredCredential::api_key(
                "brk_saved.secret".to_owned(),
                server.uri(),
            )),
            Some(StoredCredential::from(Credentials {
                access_token: "expired-access".to_owned(),
                refresh_token: Some("saved-refresh".to_owned()),
                expires_at: Utc::now() - Duration::hours(1),
                refresh_token_expires_at: None,
                token_type: "Bearer".to_owned(),
                scope: "inference:invoke".to_owned(),
                client_id: "bitrouter-cli".to_owned(),
                authorization_server: server.uri(),
                namespace_id: Some("saved-namespace".to_owned()),
                subject: None,
            })),
            None,
        ] {
            let (_directory, path) = fresh_tmp_creds_path("configured-key")?;
            let manager = Arc::new(CredentialManager::with_client(
                path.clone(),
                reqwest::Client::new(),
            ));
            if let Some(credential) = credential {
                manager.save(credential).await?;
            } else {
                std::fs::write(&path, "corrupt credentials")?;
            }
            let saved = std::fs::read(&path)?;
            let applier = BitrouterAuthApplier::new(manager.session().clone(), onboarding_hint());
            let mut target = target_for_origin(&server.uri());
            // Configured and environment keys arrive here without a per-request override.
            target.api_key = "brk_configured.secret".to_owned();
            let request = reqwest::Client::new().post(server.uri()).build()?;
            let applied = applier.apply(request, &target.model_target()).await?;
            assert_eq!(
                applied.headers()[reqwest::header::AUTHORIZATION],
                "Bearer brk_configured.secret"
            );
            assert_eq!(std::fs::read(&path)?, saved);
        }
        assert!(
            server
                .received_requests()
                .await
                .ok_or_else(|| anyhow::anyhow!("wiremock did not record requests"))?
                .is_empty(),
            "configured inference keys must not trigger stored OAuth discovery or refresh"
        );
        Ok(())
    }

    #[tokio::test]
    async fn shared_manager_single_flights_refresh_for_model_management_and_telemetry()
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

        let (_directory, path) = fresh_tmp_creds_path("shared-refresh")?;
        let manager = Arc::new(CredentialManager::with_client(
            path.clone(),
            reqwest::Client::new(),
        ));
        manager
            .save(
                bitrouter_ai::providers::hosted::credentials::StoredCredential::from(
                    bitrouter_ai::providers::hosted::credentials::Credentials {
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

        Mock::given(method("GET"))
            .and(wm_path("/v1/namespaces/ns-test/keys"))
            .and(wiremock::matchers::header(
                "authorization",
                "Bearer rotated-access",
            ))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({ "data": [] })))
            .expect(1)
            .mount(&server)
            .await;
        let management = management::ManagementClient::from_manager(Arc::clone(&manager)).await?;
        let applier = BitrouterAuthApplier::new(manager.session().clone(), onboarding_hint());
        let telemetry = CloudBearer {
            manager: Arc::clone(&manager),
            expected_origin: origin.clone(),
        };
        let request = reqwest::Client::new().post(&origin).build()?;
        let target = target_for_origin(&origin);
        let auth_target = target.model_target();
        let (applied, bearer, keys) = tokio::join!(
            applier.apply(request, &auth_target),
            telemetry.bearer(),
            management.list_keys(),
        );
        assert_eq!(
            applied?.headers()[reqwest::header::AUTHORIZATION],
            "Bearer rotated-access"
        );
        assert_eq!(bearer.as_deref(), Some("rotated-access"));
        assert!(keys?.data.is_empty());
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
            .filter(|request| request.method == "POST")
            .count();
        assert_eq!(refreshes, 1);
        Ok(())
    }

    #[test]
    fn enable_in_zero_config_noop_when_no_credentials_file() -> anyhow::Result<()> {
        let (_directory, path) = fresh_tmp_creds_path("noop")?;
        // path's parent exists; the file itself does not.
        let mut config = Config::default();
        enable_in_zero_config_with_path(&mut config, &path);
        assert!(!config.providers.contains_key(PROVIDER_ID));
        Ok(())
    }

    #[test]
    fn enable_in_zero_config_inserts_when_credentials_file_present() -> anyhow::Result<()> {
        let (_directory, path) = fresh_tmp_creds_path("inserts")?;
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
        let (_directory, path) = fresh_tmp_creds_path("already")?;
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
        let (_directory, path) = fresh_tmp_creds_path("absent")?;
        let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
        assert!(
            cloud_bearer_source(manager, "https://telemetry.bitrouter.ai")
                .await
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn cloud_bearer_source_uses_custom_exporter_same_origin_api_key() -> anyhow::Result<()> {
        let (_directory, path) = fresh_tmp_creds_path("api-key-bearer")?;
        let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
        manager
            .save(StoredCredential::api_key(
                "brk_telemetry.secret".to_owned(),
                "https://collector.example".to_owned(),
            ))
            .await?;
        let source = match cloud_bearer_source(manager, "https://collector.example/v1/traces").await
        {
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
    async fn default_first_party_exporter_uses_default_account_credential() -> anyhow::Result<()> {
        let (_directory, path) = fresh_tmp_creds_path("default-telemetry-origin")?;
        let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
        manager
            .save(StoredCredential::api_key(
                "brk_default-account.secret".to_owned(),
                "https://api.bitrouter.ai".to_owned(),
            ))
            .await?;
        let source = match cloud_bearer_source(manager, "https://telemetry.bitrouter.ai/v1/traces")
            .await
        {
            Some(source) => source,
            None => anyhow::bail!("default account credential did not produce telemetry source"),
        };
        assert_eq!(
            source.bearer().await.as_deref(),
            Some("brk_default-account.secret")
        );
        Ok(())
    }

    #[tokio::test]
    async fn custom_exporter_cannot_receive_default_account_credential() -> anyhow::Result<()> {
        let (_directory, path) = fresh_tmp_creds_path("custom-telemetry-origin")?;
        let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
        manager
            .save(StoredCredential::api_key(
                "brk_default-account.secret".to_owned(),
                "https://api.bitrouter.ai".to_owned(),
            ))
            .await?;
        let source = match cloud_bearer_source(manager, "https://collector.example/v1/traces").await
        {
            Some(source) => source,
            None => anyhow::bail!("stored credential did not produce telemetry source"),
        };
        assert!(source.bearer().await.is_none());
        Ok(())
    }

    #[tokio::test]
    async fn cloud_gateway_bearer_is_scoped_to_the_login_origin() -> anyhow::Result<()> {
        let (_directory, path) = fresh_tmp_creds_path("gateway-origin")?;
        let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
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
        let (_oauth_directory, oauth_path) = fresh_tmp_creds_path("settlement-oauth")?;
        let oauth = Arc::new(CredentialManager::with_client(
            oauth_path,
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
                .session()
                .resolve_api_key(None, Some("https://api.bitrouter.ai"))
                .await
                .is_err()
        );
        let (_api_key_directory, api_key_path) = fresh_tmp_creds_path("settlement-api-key")?;
        let api_key = Arc::new(CredentialManager::with_client(
            api_key_path,
            reqwest::Client::new(),
        ));
        api_key
            .save(StoredCredential::api_key(
                "brk_gateway.secret".to_owned(),
                "https://api.bitrouter.ai".to_owned(),
            ))
            .await?;
        let resolved = api_key
            .session()
            .resolve_api_key(None, Some("https://api.bitrouter.ai/v1"))
            .await?;
        assert_eq!(resolved.secret(), "brk_gateway.secret");
        Ok(())
    }

    #[test]
    fn cloud_bearer_debug_redacts_manager() -> anyhow::Result<()> {
        let (_directory, path) = fresh_tmp_creds_path("dbg")?;
        let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
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
        let (_directory, path) = fresh_tmp_creds_path("malformed")?;
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
