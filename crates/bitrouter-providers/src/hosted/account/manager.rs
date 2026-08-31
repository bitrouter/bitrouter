//! Safe, origin-bound resolution of hosted account credentials.

use std::collections::HashMap;
use std::fmt;
use std::path::{Path, PathBuf};

use tokio::sync::Mutex;
use url::Url;

use super::credentials::{CredentialsStore, REFRESH_WINDOW, StoredCredential};
use super::metadata::{self, AsMetadata};

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

/// Failures while accessing or resolving a hosted account credential.
#[derive(Debug, thiserror::Error)]
pub enum CredentialError {
    /// No credential is persisted at the configured path.
    #[error("no BitRouter Cloud account credential is stored")]
    NotSignedIn,
    /// The credential file could not be read, parsed, written, or removed.
    #[error("could not access BitRouter Cloud credential store: {0}")]
    Store(String),
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

/// File-backed hosted account credential resolver.
pub struct CredentialManager {
    path: PathBuf,
    client: reqwest::Client,
    metadata: Mutex<HashMap<String, AsMetadata>>,
    gate: Mutex<()>,
}

impl CredentialManager {
    /// Construct a manager using a default HTTP client.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, CredentialError> {
        let client = reqwest::Client::builder()
            .build()
            .map_err(|error| CredentialError::Store(error.to_string()))?;
        Ok(Self::with_client(path, client))
    }

    /// Construct a manager with `client`, primarily for callers controlling HTTP.
    pub fn with_client(path: impl Into<PathBuf>, client: reqwest::Client) -> Self {
        Self {
            path: path.into(),
            client,
            metadata: Mutex::new(HashMap::new()),
            gate: Mutex::new(()),
        }
    }

    /// Return the file path used for the persisted account credential.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Reload and return the currently persisted credential without refreshing it.
    pub async fn current(&self) -> Result<Option<StoredCredential>, CredentialError> {
        let _gate = self.gate.lock().await;
        Ok(Self::load_store(&self.path)?.current().cloned())
    }

    /// Persist `credential` atomically as the current account credential.
    pub async fn save(&self, credential: StoredCredential) -> Result<(), CredentialError> {
        let _gate = self.gate.lock().await;
        let mut store = Self::load_store(&self.path)?;
        store.save(credential).map_err(store_error)
    }

    /// Remove the persisted credential and return its prior value, if any.
    pub async fn clear(&self) -> Result<Option<StoredCredential>, CredentialError> {
        let _gate = self.gate.lock().await;
        let mut store = Self::load_store(&self.path)?;
        store.clear().map_err(store_error)
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

        let _gate = self.gate.lock().await;
        let mut store = Self::load_store(&self.path)?;
        let credential = store
            .current()
            .cloned()
            .ok_or(CredentialError::NotSignedIn)?;
        self.verify_origin(&credential, expected_origin)?;
        match credential {
            StoredCredential::ApiKey { api_key, .. } => Ok(ResolvedCredential {
                secret: api_key,
                source: CredentialSource::StoredApiKey,
                oauth_identity: None,
            }),
            StoredCredential::Oauth { credential } => {
                let oauth_identity = OauthIdentity {
                    authorization_server: credential.authorization_server.clone(),
                    namespace_id: credential.namespace_id.clone(),
                };
                let metadata = if credential.access_token_near_expiry(REFRESH_WINDOW) {
                    Some(self.metadata_for(&credential.authorization_server).await?)
                } else {
                    None
                };
                let secret = store
                    .current_token(&self.client, metadata.as_ref())
                    .await
                    .map_err(refresh_error)?;
                Ok(ResolvedCredential {
                    secret,
                    source: CredentialSource::StoredOauth,
                    oauth_identity: Some(oauth_identity),
                })
            }
        }
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

        let _gate = self.gate.lock().await;
        let store = Self::load_store(&self.path)?;
        let credential = store
            .current()
            .cloned()
            .ok_or(CredentialError::NotSignedIn)?;
        self.verify_origin(&credential, expected_origin)?;
        match credential {
            StoredCredential::ApiKey { api_key, .. } => Ok(ResolvedCredential {
                secret: api_key,
                source: CredentialSource::StoredApiKey,
                oauth_identity: None,
            }),
            StoredCredential::Oauth { .. } => Err(CredentialError::WrongCredentialKind),
        }
    }

    fn load_store(path: &Path) -> Result<CredentialsStore, CredentialError> {
        CredentialsStore::load(path).map_err(store_error)
    }

    fn verify_origin(
        &self,
        credential: &StoredCredential,
        expected_origin: Option<&str>,
    ) -> Result<(), CredentialError> {
        let Some(expected) = expected_origin else {
            return Ok(());
        };
        let actual = credential.base_url();
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
    ) -> Result<AsMetadata, CredentialError> {
        let origin = origin_key(authorization_server).ok_or_else(|| {
            CredentialError::Metadata(format!(
                "invalid authorization server URL '{authorization_server}'"
            ))
        })?;
        if let Some(metadata) = self.metadata.lock().await.get(&origin).cloned() {
            return Ok(metadata);
        }
        let metadata = metadata::fetch(&self.client, authorization_server)
            .await
            .map_err(|error| CredentialError::Metadata(error.to_string()))?;
        self.metadata.lock().await.insert(origin, metadata.clone());
        Ok(metadata)
    }
}

fn store_error(error: anyhow::Error) -> CredentialError {
    CredentialError::Store(error.to_string())
}

fn refresh_error(error: anyhow::Error) -> CredentialError {
    CredentialError::Refresh(error.to_string())
}

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

fn origin_key(url: &str) -> Option<String> {
    let url = Url::parse(url).ok()?;
    let host = url.host_str()?;
    let port = url.port_or_known_default()?;
    Some(format!(
        "{}://{}:{port}",
        url.scheme(),
        host.to_ascii_lowercase()
    ))
}

#[cfg(test)]
mod tests {
    use super::{CredentialError, CredentialManager, CredentialSource};
    use crate::hosted::account::credentials::{Credentials, StoredCredential};

    #[tokio::test]
    async fn explicit_api_key_bypasses_malformed_store() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("account-credentials.json");
        std::fs::write(&path, b"not json")?;
        let manager = CredentialManager::with_client(path, reqwest::Client::new());
        let resolved = manager
            .resolve_bearer(
                Some("brk_explicit.secret"),
                Some("https://api.bitrouter.ai/v1"),
            )
            .await?;
        assert_eq!(resolved.secret(), "brk_explicit.secret");
        assert_eq!(resolved.source(), CredentialSource::ExplicitApiKey);
        Ok(())
    }

    #[tokio::test]
    async fn api_key_only_rejects_oauth() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let manager = CredentialManager::with_client(
            directory.path().join("account-credentials.json"),
            reqwest::Client::new(),
        );
        let credential = Credentials {
            access_token: "access-token".to_owned(),
            refresh_token: Some("refresh-token".to_owned()),
            expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
            refresh_token_expires_at: None,
            token_type: "Bearer".to_owned(),
            scope: "inference:invoke".to_owned(),
            client_id: "bitrouter-cli".to_owned(),
            authorization_server: "https://api.bitrouter.ai".to_owned(),
            namespace_id: Some("ns-test".to_owned()),
            subject: None,
        };
        manager.save(StoredCredential::from(credential)).await?;
        let error = match manager
            .resolve_api_key(None, Some("https://api.bitrouter.ai/v1"))
            .await
        {
            Ok(_) => anyhow::bail!("OAuth unexpectedly resolved as an API key"),
            Err(error) => error,
        };
        assert!(matches!(error, CredentialError::WrongCredentialKind));
        Ok(())
    }

    #[tokio::test]
    async fn stored_credential_is_origin_confined() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let manager = CredentialManager::with_client(
            directory.path().join("account-credentials.json"),
            reqwest::Client::new(),
        );
        manager
            .save(StoredCredential::api_key(
                "brk_stored.secret".to_owned(),
                "https://api.bitrouter.ai".to_owned(),
            ))
            .await?;
        let error = match manager
            .resolve_bearer(None, Some("https://example.com/v1"))
            .await
        {
            Ok(_) => anyhow::bail!("credential unexpectedly crossed origins"),
            Err(error) => error,
        };
        assert!(matches!(error, CredentialError::OriginMismatch { .. }));
        Ok(())
    }
}
