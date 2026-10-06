//! Resolution and owned refresh/commit over an injected full-envelope store.

use super::credentials::{Credentials, REFRESH_WINDOW, StoredCredential};
use super::{metadata, tokens};
use crate::auth::store::{CredentialKey, Durability, StoreError};
use async_trait::async_trait;
use std::collections::HashMap;
use std::fmt;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use url::Url;

/// The location from which a bearer credential was selected.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialSource {
    /// A non-empty API key supplied directly by the caller.
    ExplicitApiKey,
    /// A static API key persisted in the hosted account store.
    StoredApiKey,
    /// An OAuth access token persisted in the hosted account store.
    StoredOauth,
}

/// Non-secret identity context attached to an OAuth bearer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OauthIdentity {
    authorization_server: String,
    namespace_id: Option<String>,
}

impl OauthIdentity {
    /// Return the authorization-server URL that issued the credential.
    pub fn authorization_server(&self) -> &str {
        &self.authorization_server
    }

    /// Return the namespace bound to the credential, if one was issued.
    pub fn namespace_id(&self) -> Option<&str> {
        self.namespace_id.as_deref()
    }
}

/// A bearer credential resolved for one hosted request.
pub struct ResolvedCredential {
    secret: String,
    source: CredentialSource,
    oauth_identity: Option<OauthIdentity>,
}

impl fmt::Debug for ResolvedCredential {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("ResolvedCredential")
            .field("secret", &"<redacted>")
            .field("source", &self.source)
            .field("oauth_identity", &self.oauth_identity)
            .finish()
    }
}

impl ResolvedCredential {
    /// Return the bearer secret for the request being authenticated.
    pub fn secret(&self) -> &str {
        &self.secret
    }

    /// Return how this bearer credential was selected.
    pub fn source(&self) -> CredentialSource {
        self.source
    }

    /// Return non-secret OAuth identity context, if this is an OAuth bearer.
    pub fn oauth_identity(&self) -> Option<&OauthIdentity> {
        self.oauth_identity.as_ref()
    }
}

/// Failures while accessing or resolving a selected hosted credential.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// The selected backend slot contains no credential.
    #[error("no BitRouter Cloud account credential is stored")]
    NotSignedIn,
    /// The selected backend could not load or acknowledge the credential.
    #[error("could not access BitRouter Cloud credential store: {0}")]
    Storage(StoreError),
    /// Authorization-server metadata could not be discovered.
    #[error("could not discover BitRouter Cloud authorization metadata: {0}")]
    Metadata(String),
    /// The OAuth access token could not be refreshed.
    #[error("could not refresh BitRouter Cloud OAuth credential: {0}")]
    Refresh(String),
    /// The persisted credential is bound to a different endpoint origin.
    #[error("stored BitRouter Cloud credential origin {actual} does not match {expected}")]
    OriginMismatch {
        /// Origin the caller requires for this request.
        expected: String,
        /// Origin stored with the credential.
        actual: String,
    },
    /// A stored OAuth credential cannot be used where a static key is required.
    #[error("this operation requires a static BitRouter API key; the stored credential is OAuth")]
    WrongCredentialKind,
}

/// A lease over one selected hosted credential. A staged envelope must survive
/// failed commit and be rejected after an external login/logout replaces it.
#[async_trait]
pub trait HostedCredentialTransaction: Send {
    /// Current credential, including any retained replacement.
    fn credential(&self) -> Option<&StoredCredential>;
    /// A retained identity contradiction preventing replay of old refresh tokens.
    fn rejection(&self) -> Option<&'static str>;
    /// Retain the complete replacement before persistence or bearer validation.
    fn stage(&mut self, replacement: StoredCredential, rejection: Option<&'static str>);
    /// Compare with the originally observed credential and acknowledge durability.
    async fn commit(&mut self) -> Result<Durability, StoreError>;
}

/// Caller-selected backend; absent and failed slots must remain distinguishable.
#[async_trait]
pub trait HostedCredentialStore: Send + Sync {
    /// Acquire a cancellable lease for exactly this slot, without account fallback.
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> Result<Box<dyn HostedCredentialTransaction>, StoreError>;
}

/// Native hosted authentication with no file, environment or account discovery.
/// After lease admission an owned task completes bounded refresh and commit even
/// if the caller is dropped. Durability and conflict authority belong to the store.
#[derive(Clone)]
pub struct HostedSession {
    key: CredentialKey,
    store: Arc<dyn HostedCredentialStore>,
    client: reqwest::Client,
    metadata: Arc<Mutex<HashMap<String, metadata::AsMetadata>>>,
}

impl HostedSession {
    /// Bind the session to one caller-selected slot and explicit HTTP client.
    pub fn new(
        key: CredentialKey,
        store: Arc<dyn HostedCredentialStore>,
        client: reqwest::Client,
    ) -> Self {
        Self {
            key,
            store,
            client,
            metadata: Arc::new(Mutex::new(HashMap::new())),
        }
    }

    pub(super) fn key(&self) -> &CredentialKey {
        &self.key
    }
    /// Resolve a bearer, preferring a non-empty explicit API key over the store.
    pub async fn resolve_bearer(
        &self,
        explicit_api_key: Option<&str>,
        expected_origin: Option<&str>,
    ) -> Result<ResolvedCredential, CredentialError> {
        if let Some(api_key) = explicit_api_key.filter(|api_key| !api_key.is_empty()) {
            return Ok(ResolvedCredential {
                secret: api_key.to_owned(),
                source: CredentialSource::ExplicitApiKey,
                oauth_identity: None,
            });
        }

        // Waiting for the lease remains cancellable. No refresh work has begun.
        let transaction = self
            .store
            .begin(&self.key)
            .await
            .map_err(CredentialError::Storage)?;
        let session = self.clone();
        let expected_origin = expected_origin.map(str::to_owned);
        tokio::spawn(async move {
            session
                .resolve_transaction(transaction, expected_origin.as_deref())
                .await
        })
        .await
        .map_err(|_| CredentialError::Storage(StoreError::Unavailable))?
    }

    async fn resolve_transaction(
        &self,
        mut transaction: Box<dyn HostedCredentialTransaction>,
        expected_origin: Option<&str>,
    ) -> Result<ResolvedCredential, CredentialError> {
        let credential = transaction
            .credential()
            .cloned()
            .ok_or(CredentialError::NotSignedIn)?;
        self.verify_origin(&credential, expected_origin)?;
        if let Some(reason) = transaction.rejection() {
            return Err(CredentialError::Refresh(reason.into()));
        }
        // Retry a retained rotation before any further token-endpoint exchange.
        transaction
            .commit()
            .await
            .map_err(CredentialError::Storage)?;
        match credential {
            StoredCredential::ApiKey { api_key, .. } => {
                if api_key.is_empty() {
                    return Err(CredentialError::Refresh("stored API key is empty".into()));
                }
                Ok(ResolvedCredential {
                    secret: api_key,
                    source: CredentialSource::StoredApiKey,
                    oauth_identity: None,
                })
            }
            StoredCredential::Oauth { credential } => {
                let credential = if credential.access_token_near_expiry(REFRESH_WINDOW) {
                    let (refreshed, rejection) = self.refresh(&credential).await?;
                    transaction.stage(StoredCredential::from(refreshed.clone()), rejection);
                    if let Some(reason) = rejection {
                        // Keep returned rotation material, but do not persist or
                        // dispatch a token that contradicts the selected identity.
                        return Err(CredentialError::Refresh(reason.into()));
                    }
                    transaction
                        .commit()
                        .await
                        .map_err(CredentialError::Storage)?;
                    refreshed
                } else {
                    credential
                };
                // Validate after staging/commit so even unusable rotated material
                // does not get discarded in favor of the obsolete refresh token.
                if credential.access_token.is_empty()
                    || !credential.access_token_valid()
                    || !credential.token_type.eq_ignore_ascii_case("Bearer")
                {
                    return Err(CredentialError::Refresh(
                        "stored OAuth bearer is unusable; log in again".into(),
                    ));
                }
                Ok(ResolvedCredential {
                    secret: credential.access_token,
                    source: CredentialSource::StoredOauth,
                    oauth_identity: Some(OauthIdentity {
                        authorization_server: credential.authorization_server,
                        namespace_id: credential.namespace_id,
                    }),
                })
            }
        }
    }

    async fn refresh(
        &self,
        credential: &Credentials,
    ) -> Result<(Credentials, Option<&'static str>), CredentialError> {
        let refresh_token = credential
            .refresh_token
            .as_deref()
            .filter(|token| !token.is_empty())
            .ok_or_else(|| {
                CredentialError::Refresh("no refresh token is stored; log in again".into())
            })?;
        if !credential.refresh_token_usable() {
            return Err(CredentialError::Refresh(
                "refresh token has expired; log in again".into(),
            ));
        }
        let metadata = self.metadata_for(&credential.authorization_server).await?;
        let token_set = tokio::time::timeout(
            AUTH_IO_TIMEOUT,
            tokens::refresh(
                &self.client,
                &metadata.token_endpoint,
                &credential.client_id,
                refresh_token,
                Some(&credential.scope),
            ),
        )
        .await
        .map_err(|_| CredentialError::Refresh("token exchange timed out".into()))?
        .map_err(|_| CredentialError::Refresh("token exchange failed; log in again".into()))?;
        // Keep the full hosted envelope. A generic OAuthToken cannot represent
        // namespace, issuer, granted scope or independent refresh-token expiry.
        let identity_changed = credential
            .namespace_id
            .as_ref()
            .zip(token_set.namespace_id.as_ref())
            .is_some_and(|(old, new)| old != new)
            || credential
                .subject
                .as_ref()
                .zip(token_set.subject.as_ref())
                .is_some_and(|(old, new)| old != new);
        let refreshed = Credentials {
            access_token: token_set.access_token,
            refresh_token: token_set
                .refresh_token
                .or_else(|| credential.refresh_token.clone()),
            expires_at: token_set.expires_at,
            refresh_token_expires_at: token_set
                .refresh_token_expires_at
                .or(credential.refresh_token_expires_at),
            token_type: token_set
                .token_type
                .unwrap_or_else(|| credential.token_type.clone()),
            scope: token_set.scope.unwrap_or_else(|| credential.scope.clone()),
            client_id: credential.client_id.clone(),
            authorization_server: credential.authorization_server.clone(),
            namespace_id: token_set
                .namespace_id
                .or_else(|| credential.namespace_id.clone()),
            subject: token_set.subject.or_else(|| credential.subject.clone()),
        };
        Ok((
            refreshed,
            identity_changed.then_some("refresh changed OAuth identity; log in again"),
        ))
    }

    /// Resolve a static API key, rejecting a stored OAuth credential.
    pub async fn resolve_api_key(
        &self,
        explicit_api_key: Option<&str>,
        expected_origin: Option<&str>,
    ) -> Result<ResolvedCredential, CredentialError> {
        if let Some(api_key) = explicit_api_key.filter(|api_key| !api_key.is_empty()) {
            return Ok(ResolvedCredential {
                secret: api_key.to_owned(),
                source: CredentialSource::ExplicitApiKey,
                oauth_identity: None,
            });
        }

        let transaction = self
            .store
            .begin(&self.key)
            .await
            .map_err(CredentialError::Storage)?;
        let credential = transaction
            .credential()
            .cloned()
            .ok_or(CredentialError::NotSignedIn)?;
        match credential {
            StoredCredential::Oauth { .. } => Err(CredentialError::WrongCredentialKind),
            StoredCredential::ApiKey { api_key, base_url } => {
                self.verify_base_url_origin(&base_url, expected_origin)?;
                Ok(ResolvedCredential {
                    secret: api_key,
                    source: CredentialSource::StoredApiKey,
                    oauth_identity: None,
                })
            }
        }
    }
    fn verify_origin(
        &self,
        credential: &StoredCredential,
        expected_origin: Option<&str>,
    ) -> Result<(), CredentialError> {
        self.verify_base_url_origin(credential.base_url(), expected_origin)
    }

    fn verify_base_url_origin(
        &self,
        actual: &str,
        expected_origin: Option<&str>,
    ) -> Result<(), CredentialError> {
        let Some(expected) = expected_origin else {
            return Ok(());
        };
        if origins_match(actual, expected) {
            return Ok(());
        }
        Err(CredentialError::OriginMismatch {
            expected: expected.to_owned(),
            actual: actual.to_owned(),
        })
    }

    async fn metadata_for(
        &self,
        authorization_server: &str,
    ) -> Result<metadata::AsMetadata, CredentialError> {
        // Issuer paths can advertise different token endpoints on the same origin.
        let key = metadata::metadata_url(authorization_server)
            .map_err(|_| CredentialError::Metadata("invalid authorization server URL".into()))?;
        if let Some(metadata) = self.metadata.lock().await.get(&key).cloned() {
            return Ok(metadata);
        }
        let metadata = tokio::time::timeout(
            AUTH_IO_TIMEOUT,
            metadata::fetch(&self.client, authorization_server),
        )
        .await
        .map_err(|_| CredentialError::Metadata("metadata discovery timed out".into()))?
        .map_err(|_| CredentialError::Metadata("metadata discovery failed".into()))?;
        self.metadata.lock().await.insert(key, metadata.clone());
        Ok(metadata)
    }
}

const AUTH_IO_TIMEOUT: Duration = Duration::from_secs(30);

fn origins_match(actual: &str, expected: &str) -> bool {
    let (Ok(actual), Ok(expected)) = (Url::parse(actual), Url::parse(expected)) else {
        return false;
    };
    actual.scheme() == expected.scheme()
        && actual
            .host_str()
            .zip(expected.host_str())
            .is_some_and(|(actual, expected)| actual.eq_ignore_ascii_case(expected))
        && actual.port_or_known_default() == expected.port_or_known_default()
}
