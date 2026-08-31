//! Request-time authentication for the hosted BitRouter provider.

use std::sync::Arc;

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderValue};

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::auth::{AppliedAuth, CredentialAuthority};
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use super::account::manager::{CredentialError, CredentialManager, CredentialSource};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "bitrouter";

/// Onboarding text for callers without a configured hosted credential.
pub fn onboarding_hint() -> &'static str {
    "no BitRouter Cloud credential — run `bitrouter cloud login` or set BITROUTER_API_KEY=brk_…"
}

/// Request-time authentication for the hosted BitRouter provider.
pub struct BitrouterAuthApplier {
    manager: Arc<CredentialManager>,
}

struct ResolvedAuth {
    bearer: String,
    authority: Option<CredentialAuthority>,
}

impl BitrouterAuthApplier {
    /// Construct an applier backed by the shared account credential manager.
    pub fn new(manager: Arc<CredentialManager>) -> Self {
        Self { manager }
    }

    async fn resolve_auth(
        &self,
        explicit_api_key: &str,
        expected_origin: &str,
    ) -> Result<ResolvedAuth> {
        let credential = self
            .manager
            .resolve_bearer(Some(explicit_api_key), Some(expected_origin))
            .await
            .map_err(map_credential_error)?;
        let authority = match credential.source() {
            CredentialSource::ExplicitApiKey => Some(CredentialAuthority::derive(
                "bitrouter-cloud/inline-api-key",
                credential.secret(),
            )),
            CredentialSource::StoredApiKey => Some(CredentialAuthority::derive(
                "bitrouter-cloud/stored-api-key",
                credential.secret(),
            )),
            CredentialSource::StoredOauth => {
                oauth_authority(credential.oauth_identity().ok_or_else(|| {
                    BitrouterError::internal("OAuth credential missing identity")
                })?)?
            }
        };
        Ok(ResolvedAuth {
            bearer: credential.secret().to_owned(),
            authority,
        })
    }
}

fn oauth_authority(
    identity: &super::account::manager::OauthIdentity,
) -> Result<Option<CredentialAuthority>> {
    let Some(namespace_id) = identity
        .namespace_id()
        .filter(|namespace_id| !namespace_id.is_empty())
    else {
        return Ok(None);
    };
    let issuer = url::Url::parse(identity.authorization_server().trim_end_matches('/')).map_err(
        |error| {
            BitrouterError::internal(format!(
                "invalid BitRouter Cloud OAuth authorization server: {error}"
            ))
        },
    )?;
    Ok(Some(CredentialAuthority::derive_scoped(
        "bitrouter-cloud/oauth-namespace",
        issuer.as_str().trim_end_matches('/'),
        namespace_id,
    )))
}

fn map_credential_error(error: CredentialError) -> BitrouterError {
    match error {
        CredentialError::NotSignedIn => BitrouterError::Upstream {
            status: 401,
            message: onboarding_hint().to_owned(),
        },
        CredentialError::Refresh(message) => BitrouterError::Upstream {
            status: 401,
            message: format!("BitRouter Cloud token refresh failed: {message}"),
        },
        CredentialError::Metadata(message) => BitrouterError::Upstream {
            status: 502,
            message: format!("fetching BitRouter Cloud authorization metadata: {message}"),
        },
        CredentialError::OriginMismatch { expected, actual } => BitrouterError::Upstream {
            status: 401,
            message: format!(
                "stored BitRouter Cloud credential origin {actual} does not match {expected}"
            ),
        },
        CredentialError::Store(message) => BitrouterError::internal(format!(
            "accessing BitRouter Cloud credential store: {message}"
        )),
        CredentialError::WrongCredentialKind => {
            BitrouterError::internal("stored OAuth credential was rejected by bearer resolution")
        }
    }
}

#[async_trait]
impl AuthApplier for BitrouterAuthApplier {
    async fn apply(
        &self,
        request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        Ok(self
            .apply_with_authority(request, target)
            .await?
            .into_request())
    }

    async fn apply_with_authority(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<AppliedAuth> {
        if request.headers().contains_key(AUTHORIZATION) {
            return Ok(AppliedAuth::unproven(request));
        }
        let request_url = request.url().to_string();
        let auth = self
            .resolve_auth(target.effective_api_key(), &request_url)
            .await?;
        let value = HeaderValue::from_str(&format!("Bearer {}", auth.bearer)).map_err(|error| {
            BitrouterError::internal(format!(
                "invalid BitRouter Cloud bearer for Authorization: {error}"
            ))
        })?;
        request.headers_mut().insert(AUTHORIZATION, value);
        Ok(match auth.authority {
            Some(authority) => AppliedAuth::proven(request, authority),
            None => AppliedAuth::unproven(request),
        })
    }

    async fn continuation_authority(
        &self,
        target: &RoutingTarget,
    ) -> Result<Option<CredentialAuthority>> {
        Ok(self
            .resolve_auth(target.effective_api_key(), target.effective_api_base())
            .await?
            .authority)
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_sdk::BitrouterError;
    use bitrouter_sdk::language_model::AuthApplier;
    use bitrouter_sdk::language_model::types::{ApiProtocol, RoutingTarget};
    use chrono::{Duration, Utc};
    use reqwest::header::{AUTHORIZATION, HeaderValue};
    use serde_json::json;
    use wiremock::matchers::{body_string_contains, method, path as wm_path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    use super::{BitrouterAuthApplier, PROVIDER_ID};
    use crate::hosted::account::credentials::{Credentials, StoredCredential};
    use crate::hosted::account::manager::CredentialManager;

    fn tmp_creds_path(label: &str) -> anyhow::Result<std::path::PathBuf> {
        let directory = tempfile::Builder::new()
            .prefix(&format!("bitrouter-hosted-applier-{label}-"))
            .tempdir()?;
        Ok(directory.keep().join("account-credentials.json"))
    }

    fn target_with_api_key(key: &str) -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.to_owned(),
            service_id: "gpt-4o".to_owned(),
            api_base: "https://api.bitrouter.ai/v1".to_owned(),
            api_key: key.to_owned(),
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

    fn empty_request() -> anyhow::Result<reqwest::Request> {
        Ok(reqwest::Client::new()
            .post("https://api.bitrouter.ai/v1/chat/completions")
            .build()?)
    }

    fn target_for_origin(origin: &str) -> RoutingTarget {
        let mut target = target_with_api_key("");
        target.api_base = origin.to_owned();
        target
    }

    fn oauth_credential(
        authorization_server: &str,
        namespace_id: Option<&str>,
        access_token: &str,
        refresh_token: &str,
    ) -> Credentials {
        Credentials {
            access_token: access_token.to_owned(),
            refresh_token: Some(refresh_token.to_owned()),
            expires_at: Utc::now() + Duration::hours(1),
            refresh_token_expires_at: None,
            token_type: "Bearer".to_owned(),
            scope: "inference:invoke".to_owned(),
            client_id: "bitrouter-cli".to_owned(),
            authorization_server: authorization_server.to_owned(),
            namespace_id: namespace_id.map(str::to_owned),
            subject: Some("user-42".to_owned()),
        }
    }

    #[tokio::test]
    async fn preserves_request_authorization_header() -> anyhow::Result<()> {
        let path = tmp_creds_path("raw-authorization")?;
        std::fs::write(&path, b"malformed")?;
        let manager = CredentialManager::with_client(path, reqwest::Client::new());
        let applier = BitrouterAuthApplier::new(Arc::new(manager));
        let mut request = empty_request()?;
        request.headers_mut().insert(
            AUTHORIZATION,
            HeaderValue::from_static("Bearer raw-request-token"),
        );
        let applied = applier
            .apply(request, &target_with_api_key("brk_config.secret"))
            .await?;
        assert_eq!(applied.headers()[AUTHORIZATION], "Bearer raw-request-token");
        Ok(())
    }

    #[tokio::test]
    async fn explicit_target_key_bypasses_bad_oauth_store() -> anyhow::Result<()> {
        let path = tmp_creds_path("explicit-bypass")?;
        std::fs::write(&path, b"malformed")?;
        let manager = CredentialManager::with_client(path, reqwest::Client::new());
        let applier = BitrouterAuthApplier::new(Arc::new(manager));
        let applied = applier
            .apply(empty_request()?, &target_with_api_key("brk_config.secret"))
            .await?;
        assert_eq!(applied.headers()[AUTHORIZATION], "Bearer brk_config.secret");
        Ok(())
    }

    #[tokio::test]
    async fn applies_stored_api_key_for_request_origin() -> anyhow::Result<()> {
        let path = tmp_creds_path("stored-api-key")?;
        let manager = Arc::new(CredentialManager::with_client(path, reqwest::Client::new()));
        manager
            .save(StoredCredential::api_key(
                "brk_stored.secret".to_owned(),
                "https://api.bitrouter.ai".to_owned(),
            ))
            .await?;
        let applier = BitrouterAuthApplier::new(manager);
        let applied = applier
            .apply(empty_request()?, &target_with_api_key(""))
            .await?;
        assert_eq!(applied.headers()[AUTHORIZATION], "Bearer brk_stored.secret");
        Ok(())
    }

    #[tokio::test]
    async fn oauth_authority_survives_token_rotation_for_same_issuer_and_namespace()
    -> anyhow::Result<()> {
        let origin = "https://issuer.example";
        let manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("oauth-rotation-authority")?,
            reqwest::Client::new(),
        ));
        let applier = BitrouterAuthApplier::new(Arc::clone(&manager));
        let target = target_for_origin(origin);
        manager
            .save(StoredCredential::from(oauth_credential(
                origin,
                Some("ns-one"),
                "access-before",
                "refresh-before",
            )))
            .await?;
        let before = applier.continuation_authority(&target).await?;
        manager
            .save(StoredCredential::from(oauth_credential(
                origin,
                Some("ns-one"),
                "access-after",
                "refresh-after",
            )))
            .await?;
        let after = applier.continuation_authority(&target).await?;
        assert_eq!(before, after);
        Ok(())
    }

    #[tokio::test]
    async fn oauth_authority_separates_namespaces_for_same_issuer() -> anyhow::Result<()> {
        let origin = "https://issuer.example";
        let first_manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("oauth-first-namespace-authority")?,
            reqwest::Client::new(),
        ));
        first_manager
            .save(StoredCredential::from(oauth_credential(
                origin,
                Some("ns-one"),
                "access-first",
                "refresh-first",
            )))
            .await?;
        let first = BitrouterAuthApplier::new(first_manager)
            .continuation_authority(&target_for_origin(origin))
            .await?;

        let second_manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("oauth-second-namespace-authority")?,
            reqwest::Client::new(),
        ));
        second_manager
            .save(StoredCredential::from(oauth_credential(
                origin,
                Some("ns-two"),
                "access-second",
                "refresh-second",
            )))
            .await?;
        let second = BitrouterAuthApplier::new(second_manager)
            .continuation_authority(&target_for_origin(origin))
            .await?;
        assert_ne!(first, second);
        Ok(())
    }

    #[tokio::test]
    async fn oauth_authority_separates_issuers_for_same_namespace() -> anyhow::Result<()> {
        let first_origin = "https://issuer-one.example";
        let second_origin = "https://issuer-two.example";
        let first_manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("oauth-first-issuer-authority")?,
            reqwest::Client::new(),
        ));
        first_manager
            .save(StoredCredential::from(oauth_credential(
                first_origin,
                Some("ns-one"),
                "access-first",
                "refresh-first",
            )))
            .await?;
        let first = BitrouterAuthApplier::new(first_manager)
            .continuation_authority(&target_for_origin(first_origin))
            .await?;

        let second_manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("oauth-second-issuer-authority")?,
            reqwest::Client::new(),
        ));
        second_manager
            .save(StoredCredential::from(oauth_credential(
                second_origin,
                Some("ns-one"),
                "access-second",
                "refresh-second",
            )))
            .await?;
        let second = BitrouterAuthApplier::new(second_manager)
            .continuation_authority(&target_for_origin(second_origin))
            .await?;
        assert_ne!(first, second);
        Ok(())
    }

    #[tokio::test]
    async fn oauth_without_namespace_has_no_continuation_authority() -> anyhow::Result<()> {
        let origin = "https://issuer.example";
        let manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("oauth-missing-namespace")?,
            reqwest::Client::new(),
        ));
        manager
            .save(StoredCredential::from(oauth_credential(
                origin,
                None,
                "access-token",
                "refresh-token",
            )))
            .await?;
        assert!(
            BitrouterAuthApplier::new(manager)
                .continuation_authority(&target_for_origin(origin))
                .await?
                .is_none()
        );
        Ok(())
    }

    #[tokio::test]
    async fn explicit_authority_tracks_the_effective_api_key() -> anyhow::Result<()> {
        let applier = BitrouterAuthApplier::new(Arc::new(CredentialManager::with_client(
            tmp_creds_path("explicit-authority")?,
            reqwest::Client::new(),
        )));
        let base = applier
            .continuation_authority(&target_with_api_key("brk_base.secret"))
            .await?;
        let mut overridden = target_with_api_key("brk_base.secret");
        overridden.api_key_override = Some("brk_override.secret".to_owned());
        let override_authority = applier.continuation_authority(&overridden).await?;
        let effective_key = applier
            .continuation_authority(&target_with_api_key("brk_override.secret"))
            .await?;
        assert_ne!(base, override_authority);
        assert_eq!(override_authority, effective_key);
        Ok(())
    }

    #[tokio::test]
    async fn missing_credential_maps_to_onboarding_401() -> anyhow::Result<()> {
        let manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("missing")?,
            reqwest::Client::new(),
        ));
        let error = match BitrouterAuthApplier::new(manager)
            .apply(empty_request()?, &target_with_api_key(""))
            .await
        {
            Ok(_) => anyhow::bail!("missing credential unexpectedly authenticated a request"),
            Err(error) => error,
        };
        match error {
            BitrouterError::Upstream { status, message } => {
                assert_eq!(status, 401);
                assert_eq!(message, super::onboarding_hint());
            }
            other => anyhow::bail!("expected upstream 401, got {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_store_maps_to_internal_error() -> anyhow::Result<()> {
        let path = tmp_creds_path("corrupt")?;
        std::fs::write(&path, b"not-json")?;
        let error = match BitrouterAuthApplier::new(Arc::new(CredentialManager::with_client(
            path,
            reqwest::Client::new(),
        )))
        .apply(empty_request()?, &target_with_api_key(""))
        .await
        {
            Ok(_) => anyhow::bail!("corrupt credential store unexpectedly authenticated a request"),
            Err(error) => error,
        };
        assert!(matches!(error, BitrouterError::Internal(_)));
        Ok(())
    }

    #[tokio::test]
    async fn metadata_failure_maps_to_upstream_502() -> anyhow::Result<()> {
        let server = MockServer::start().await;
        let origin = server.uri();
        Mock::given(method("GET"))
            .and(wm_path("/.well-known/oauth-authorization-server"))
            .respond_with(ResponseTemplate::new(500))
            .mount(&server)
            .await;
        let manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("metadata-failure")?,
            reqwest::Client::new(),
        ));
        manager
            .save(StoredCredential::from(Credentials {
                access_token: "stale-access".to_owned(),
                refresh_token: Some("refresh-token".to_owned()),
                expires_at: Utc::now() + Duration::seconds(10),
                refresh_token_expires_at: None,
                token_type: "Bearer".to_owned(),
                scope: "inference:invoke".to_owned(),
                client_id: "bitrouter-cli".to_owned(),
                authorization_server: origin.clone(),
                namespace_id: Some("ns-test".to_owned()),
                subject: None,
            }))
            .await?;
        let request = reqwest::Client::new().post(&origin).build()?;
        let error = match BitrouterAuthApplier::new(manager)
            .apply(request, &target_for_origin(&origin))
            .await
        {
            Ok(_) => anyhow::bail!("metadata failure unexpectedly authenticated a request"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            BitrouterError::Upstream { status: 502, .. }
        ));
        Ok(())
    }

    #[tokio::test]
    async fn refresh_failure_maps_to_upstream_401() -> anyhow::Result<()> {
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
            .respond_with(ResponseTemplate::new(400).set_body_json(json!({
                "error": "invalid_grant",
            })))
            .mount(&server)
            .await;
        let manager = Arc::new(CredentialManager::with_client(
            tmp_creds_path("refresh-failure")?,
            reqwest::Client::new(),
        ));
        manager
            .save(StoredCredential::from(Credentials {
                access_token: "stale-access".to_owned(),
                refresh_token: Some("refresh-token".to_owned()),
                expires_at: Utc::now() + Duration::seconds(10),
                refresh_token_expires_at: None,
                token_type: "Bearer".to_owned(),
                scope: "inference:invoke".to_owned(),
                client_id: "bitrouter-cli".to_owned(),
                authorization_server: origin.clone(),
                namespace_id: Some("ns-test".to_owned()),
                subject: None,
            }))
            .await?;
        let request = reqwest::Client::new().post(&origin).build()?;
        let error = match BitrouterAuthApplier::new(manager)
            .apply(request, &target_for_origin(&origin))
            .await
        {
            Ok(_) => anyhow::bail!("refresh failure unexpectedly authenticated a request"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            BitrouterError::Upstream { status: 401, .. }
        ));
        Ok(())
    }
}
