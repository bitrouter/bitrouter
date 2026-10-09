//! Request-time authentication for the hosted BitRouter provider.

use async_trait::async_trait;
use reqwest::header::{AUTHORIZATION, HeaderValue};

use crate::auth::AuthApplier;
use crate::auth::{AppliedAuth, CredentialAuthority};
use crate::error::{ModelError, Result};
use crate::target::ModelTarget;

use super::session::{CredentialError, CredentialSource, HostedSession, OauthIdentity};

/// Request-time authentication for the hosted BitRouter provider.
pub struct BitrouterAuthApplier {
    session: HostedSession,
    onboarding_hint: String,
}

struct ResolvedAuth {
    bearer: String,
    authority: Option<CredentialAuthority>,
}

impl BitrouterAuthApplier {
    /// Construct an applier with an explicit hosted session and application onboarding text.
    pub fn new(session: HostedSession, onboarding_hint: String) -> Self {
        Self {
            session,
            onboarding_hint,
        }
    }

    async fn resolve_auth(
        &self,
        explicit_api_key: &str,
        expected_origin: &str,
    ) -> Result<ResolvedAuth> {
        let credential = self
            .session
            .resolve_bearer(Some(explicit_api_key), Some(expected_origin))
            .await
            .map_err(|error| map_credential_error(error, &self.onboarding_hint))?;
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
                    ModelError::configuration("OAuth credential missing identity")
                })?)?
            }
        };
        Ok(ResolvedAuth {
            bearer: credential.secret().to_owned(),
            authority,
        })
    }
    async fn resolve_target_auth(
        &self,
        target: &ModelTarget,
        origin: &str,
    ) -> Result<ResolvedAuth> {
        if let Some(key) = target.explicit_credential() {
            return self.resolve_auth(key, origin).await;
        }
        let account = target
            .account_label
            .as_deref()
            .unwrap_or(crate::auth::store::DEFAULT_ACCOUNT);
        if self.session.key().provider != target.provider_name
            || self.session.key().account != account
        {
            return Err(ModelError::configuration(
                "hosted auth session does not match the selected provider/account",
            ));
        }
        match self.session.resolve_bearer(None, Some(origin)).await {
            Err(CredentialError::NotSignedIn) if target.fallback_credential().is_some() => {
                self.resolve_auth(&target.api_key, origin).await
            }
            Err(error) => Err(map_credential_error(error, &self.onboarding_hint)),
            Ok(credential) => {
                let authority = match credential.source() {
                    CredentialSource::StoredApiKey => Some(CredentialAuthority::derive(
                        "bitrouter-cloud/stored-api-key",
                        credential.secret(),
                    )),
                    CredentialSource::StoredOauth => {
                        oauth_authority(credential.oauth_identity().ok_or_else(|| {
                            ModelError::configuration("OAuth credential missing identity")
                        })?)?
                    }
                    CredentialSource::ExplicitApiKey => {
                        return Err(ModelError::configuration(
                            "stored credential resolved as explicit",
                        ));
                    }
                };
                Ok(ResolvedAuth {
                    bearer: credential.secret().to_owned(),
                    authority,
                })
            }
        }
    }
}

fn oauth_authority(identity: &OauthIdentity) -> Result<Option<CredentialAuthority>> {
    let Some(namespace_id) = identity
        .namespace_id()
        .filter(|namespace_id| !namespace_id.is_empty())
    else {
        return Ok(None);
    };
    let issuer = url::Url::parse(identity.authorization_server().trim_end_matches('/')).map_err(
        |error| {
            ModelError::configuration(format!(
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

fn map_credential_error(error: CredentialError, onboarding_hint: &str) -> ModelError {
    match error {
        CredentialError::NotSignedIn => ModelError::Provider {
            status: 401,
            message: onboarding_hint.to_owned(),
        },
        CredentialError::Refresh(message) => ModelError::Provider {
            status: 401,
            message: format!("BitRouter Cloud token refresh failed: {message}"),
        },
        CredentialError::Metadata(message) => ModelError::Provider {
            status: 502,
            message: format!("fetching BitRouter Cloud authorization metadata: {message}"),
        },
        CredentialError::OriginMismatch { expected, actual } => ModelError::Provider {
            status: 401,
            message: format!(
                "stored BitRouter Cloud credential origin {actual} does not match {expected}"
            ),
        },
        CredentialError::Storage(failure) => ModelError::CredentialStorage { failure },
        CredentialError::WrongCredentialKind => {
            ModelError::configuration("stored OAuth credential was rejected by bearer resolution")
        }
    }
}

#[async_trait]
impl AuthApplier for BitrouterAuthApplier {
    async fn apply(
        &self,
        request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<reqwest::Request> {
        Ok(self
            .apply_with_authority(request, target)
            .await?
            .into_request())
    }

    async fn apply_with_authority(
        &self,
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<AppliedAuth> {
        if request.headers().contains_key(AUTHORIZATION) {
            return Ok(AppliedAuth::unproven(request));
        }
        let request_url = request.url().to_string();
        let auth = self.resolve_target_auth(target, &request_url).await?;
        let value = HeaderValue::from_str(&format!("Bearer {}", auth.bearer)).map_err(|error| {
            ModelError::configuration(format!(
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
        target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        Ok(self
            .resolve_target_auth(target, target.api_base.as_str())
            .await?
            .authority)
    }
}
