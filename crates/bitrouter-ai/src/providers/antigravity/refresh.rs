//! Confidential Google refresh over explicitly supplied client metadata/secrets.

use crate::auth::credentials::OAuthToken;
use crate::auth::oauth::{OAuthError, refresh_with_client_secret};
use crate::auth::store::OAuthRefresher;
use crate::error::{ModelError, Result};
use async_trait::async_trait;
use std::sync::{Arc, Mutex};

/// Selected-account refresh with ordered client-secret probes and a successful-secret cache.
/// Secret discovery is an explicit caller callback, invoked only when refresh needs it.
pub struct AntigravityRefresher {
    http: reqwest::Client,
    endpoint: String,
    client_id: String,
    secret_source: Arc<dyn Fn() -> Result<Vec<String>> + Send + Sync>,
    working_secret: Mutex<Option<String>>,
}
impl AntigravityRefresher {
    /// Bind explicit HTTP/client metadata and the caller-permitted secret source.
    pub fn new(
        http: reqwest::Client,
        endpoint: impl Into<String>,
        client_id: impl Into<String>,
        secret_source: Arc<dyn Fn() -> Result<Vec<String>> + Send + Sync>,
    ) -> Self {
        Self {
            http,
            endpoint: endpoint.into(),
            client_id: client_id.into(),
            secret_source,
            working_secret: Mutex::new(None),
        }
    }
}
#[async_trait]
impl OAuthRefresher for AntigravityRefresher {
    async fn refresh(&self, token: &OAuthToken) -> Result<OAuthToken> {
        self.refresh_via_agy(token).await
    }
}

impl AntigravityRefresher {
    /// Refresh the Google OAuth token using the confidential `agy` client. The
    /// caller provides candidate client secrets. Only `invalid_client` permits
    /// another candidate; other failures abort the grant for this account.
    async fn refresh_via_agy(&self, token: &OAuthToken) -> Result<OAuthToken> {
        let secrets = self.secret_candidates()?;
        let mut last: Option<OAuthError> = None;
        for secret in secrets {
            match refresh_with_client_secret(
                &self.http,
                &self.endpoint,
                &self.client_id,
                &secret,
                token,
            )
            .await
            {
                Ok(refreshed) => {
                    if let Ok(mut guard) = self.working_secret.lock() {
                        *guard = Some(secret);
                    }
                    return Ok(refreshed);
                }
                Err(e) if matches!(&e, OAuthError::OAuthError { error, .. } if error == "invalid_client") => {
                    last = Some(e)
                }
                Err(error) => return Err(refresh_error(error)),
            }
        }
        Err(match last {
            Some(e) => refresh_error(e),
            None => ModelError::Provider {
                status: 401,
                message: "no client secret supplied for the selected Google AI refresh grant"
                    .into(),
            },
        })
    }

    /// The secret candidates to try: the last-working one first (cheap), else
    /// the explicitly injected source. AI discovers no local binary or environment.
    fn secret_candidates(&self) -> Result<Vec<String>> {
        if let Ok(guard) = self.working_secret.lock()
            && let Some(s) = guard.as_ref()
        {
            return Ok(vec![s.clone()]);
        }
        (self.secret_source)()
    }
}

fn refresh_error(error: OAuthError) -> ModelError {
    match error {
        OAuthError::OAuthError { error, .. } => ModelError::Provider {
            status: 401,
            message: format!("Google AI OAuth refresh failed ({error}); explicitly reauthenticate the selected account"),
        },
        OAuthError::MissingRefreshToken => ModelError::Provider {
            status: 401,
            message: "selected Google AI credential has no refresh grant; explicitly reauthenticate this account".into(),
        },
        other => ModelError::Provider { status: 502, message: format!("Google AI OAuth refresh transport/protocol error: {other}") },
    }
}
