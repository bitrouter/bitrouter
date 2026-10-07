//! File-backed account assembly. AI owns hosted bearer resolution and refresh.

use super::credentials::CredentialsStore;
use super::transaction::{AccountTransaction, FileHostedStore};
use bitrouter_ai::auth::store::{CredentialKey, DEFAULT_ACCOUNT, StoreError};
use bitrouter_ai::providers::hosted::credentials::StoredCredential;
use bitrouter_ai::providers::hosted::session::{CredentialError, HostedSession};
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Application account persistence and assembly of a single AI hosted session.
#[derive(Clone)]
pub struct CredentialManager {
    path: PathBuf,
    session: HostedSession,
}

impl CredentialManager {
    /// Construct the application's bounded HTTP client without reading the file.
    pub fn new(path: impl Into<PathBuf>) -> Result<Self, CredentialError> {
        let client = reqwest::Client::builder()
            .timeout(std::time::Duration::from_secs(30))
            .build()
            .map_err(|_| CredentialError::Storage(StoreError::Unavailable))?;
        Ok(Self::with_client(path, client))
    }

    /// Assemble explicit file storage and HTTP inputs for the AI session.
    pub fn with_client(path: impl Into<PathBuf>, client: reqwest::Client) -> Self {
        let path = path.into();
        let key = CredentialKey {
            provider: bitrouter_ai::providers::hosted::PROVIDER_ID.into(),
            account: DEFAULT_ACCOUNT.into(),
        };
        let session = HostedSession::new(key, Arc::new(FileHostedStore::new(path.clone())), client);
        Self { path, session }
    }

    /// Return the file path selected by the application.
    pub fn path(&self) -> &Path {
        &self.path
    }

    /// Borrow the shared AI resolver used by model, telemetry and Cloud consumers.
    pub fn session(&self) -> &HostedSession {
        &self.session
    }

    /// Read persisted state without refreshing or substituting a pending envelope.
    pub async fn current(&self) -> Result<Option<StoredCredential>, CredentialError> {
        let _lease = AccountTransaction::begin(&self.path)
            .await
            .map_err(CredentialError::Storage)?;
        Ok(Self::load_store(&self.path)?.current().cloned())
    }

    /// Atomically persist an application login.
    pub async fn save(&self, credential: StoredCredential) -> Result<(), CredentialError> {
        Self::load_store(&self.path)?
            .save(credential)
            .map_err(store_error)
    }

    /// Remove the persisted login without resurrecting an in-flight rotation.
    pub async fn clear(&self) -> Result<Option<StoredCredential>, CredentialError> {
        Self::load_store(&self.path)?.clear().map_err(store_error)
    }

    fn load_store(path: &Path) -> Result<CredentialsStore, CredentialError> {
        CredentialsStore::load(path).map_err(store_error)
    }
}

fn store_error(_error: anyhow::Error) -> CredentialError {
    CredentialError::Storage(StoreError::Unavailable)
}

#[cfg(test)]
mod transaction_tests;

#[cfg(test)]
mod tests {
    use super::CredentialManager;
    use bitrouter_ai::providers::hosted::credentials::{Credentials, StoredCredential};
    use bitrouter_ai::providers::hosted::session::{CredentialError, CredentialSource};

    #[tokio::test]
    async fn explicit_api_key_bypasses_malformed_store() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("account-credentials.json");
        std::fs::write(&path, b"not json")?;
        let manager = CredentialManager::with_client(path, reqwest::Client::new());
        let resolved = manager
            .session()
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
            .session()
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
    async fn api_key_only_rejects_wrong_origin_oauth_as_wrong_kind() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let manager = CredentialManager::with_client(
            directory.path().join("account-credentials.json"),
            reqwest::Client::new(),
        );
        manager
            .save(StoredCredential::from(Credentials {
                access_token: "access-token".to_owned(),
                refresh_token: Some("refresh-token".to_owned()),
                expires_at: chrono::Utc::now() + chrono::Duration::minutes(10),
                refresh_token_expires_at: None,
                token_type: "Bearer".to_owned(),
                scope: "inference:invoke".to_owned(),
                client_id: "bitrouter-cli".to_owned(),
                authorization_server: "https://other.example".to_owned(),
                namespace_id: Some("ns-test".to_owned()),
                subject: None,
            }))
            .await?;

        let error = match manager
            .session()
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
            .session()
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
