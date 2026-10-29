//! Authentication assembled solely from an immutable publication and shared slots.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, ensure};
use async_trait::async_trait;
use bitrouter_ai::types::ApiProtocol;

use crate::gateway::model::error::{ModelError, Result};
use crate::gateway::model::protocol::{self, Transport};
use crate::gateway::model::target::ModelTarget;
use crate::gateway::providers;
use crate::gateway::providers::auth::{
    AppliedAuth, AuthApplier, AuthAppliers, ContinuationAuthority, CredentialAuthority,
    credentials::Credential,
    oauth::{REFRESH_WINDOW, RefreshGrant},
    store::{
        CredentialKey, CredentialStore, CredentialTransaction, DEFAULT_ACCOUNT, OAuthSession,
        StoreError,
    },
};

use super::credentials::{Credentials, observation::Observation};
use super::publication::{CredentialBinding, PreparedConfiguration};

#[cfg(test)]
pub(super) mod tests;

/// Build one complete registry before publishing a serving snapshot. Every
/// provider receives an applier, so the static ambient fallback is unreachable.
pub(super) fn build(
    prepared: &PreparedConfiguration,
    bindings: &[CredentialBinding],
    credentials: Credentials,
) -> anyhow::Result<AuthAppliers> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .redirect(reqwest::redirect::Policy::none())
        .retry(reqwest::retry::never())
        .build()
        .context("building shared credential HTTP client")?;
    let mut registry = AuthAppliers::new();
    for provider in prepared.config.providers.keys() {
        let mechanism = prepared
            .authentication
            .get(provider)
            .context("shared authentication mechanism is unavailable")?
            .clone();
        let accounts: HashMap<_, _> = bindings
            .iter()
            .filter(|binding| binding.provider == *provider)
            .map(|binding| (binding.account.clone(), binding.slot()))
            .collect();
        ensure!(
            !accounts.is_empty(),
            "shared provider has no account bindings"
        );
        let store: Arc<dyn CredentialStore> = Arc::new(BoundStore {
            credentials: credentials.clone(),
            mechanism: mechanism.clone(),
            accounts,
        });
        let inner: Arc<dyn AuthApplier> = match mechanism.as_str() {
            "anthropic" => Arc::new(providers::anthropic::AnthropicApiKeyApplier::new(store)),
            "github-copilot" => Arc::new(providers::copilot::CopilotAuthApplier::new(
                client.clone(),
                providers::copilot::exchange::TOKEN_EXCHANGE_URL,
                store,
            )),
            "claude-code" | "openai-codex" | "supergrok" => {
                let (endpoint, client_id) = if mechanism == "supergrok" {
                    (
                        providers::supergrok::TOKEN_ENDPOINT.to_owned(),
                        providers::supergrok::CLIENT_ID.to_owned(),
                    )
                } else {
                    let registration = providers::login::find(if mechanism == "claude-code" {
                        "anthropic"
                    } else {
                        &mechanism
                    })
                    .context("shared OAuth registration is unavailable")?;
                    (
                        registration.auth.token_endpoint,
                        registration.auth.client_id,
                    )
                };
                let session = OAuthSession::new_inline(
                    store,
                    Arc::new(RefreshGrant::new(client.clone(), endpoint, client_id)),
                    REFRESH_WINDOW,
                );
                match mechanism.as_str() {
                    "claude-code" => {
                        Arc::new(providers::claude_code::ClaudeCodeAuthApplier::new(session))
                    }
                    "openai-codex" => {
                        Arc::new(providers::codex::OpenAiCodexAuthApplier::new(session))
                    }
                    _ => Arc::new(providers::supergrok::SuperGrokAuthApplier::new(session)),
                }
            }
            _ => Arc::new(StaticKey { store }),
        };
        registry.register(provider, Arc::new(SharedApplier { mechanism, inner }));
    }
    Ok(registry)
}

pub(super) fn mechanism(source: &serde_json::Value, provider: &str) -> anyhow::Result<String> {
    let mut current = provider;
    // Publication validation already rejects cycles and excessive depth. Keep
    // this boundary bounded even when called directly by application assembly.
    for _ in 0..=32 {
        if matches!(
            current,
            "anthropic" | "github-copilot" | "claude-code" | "openai-codex" | "supergrok"
        ) {
            return Ok(current.into());
        }
        match source
            .get("providers")
            .and_then(|providers| providers.get(current))
            .and_then(|provider| provider.get("derives"))
            .and_then(serde_json::Value::as_str)
        {
            Some(parent) => current = parent,
            None => return Ok(provider.into()),
        }
    }
    anyhow::bail!("shared authentication inheritance exceeds supported depth")
}

struct BoundStore {
    credentials: Credentials,
    mechanism: String,
    accounts: HashMap<String, CredentialKey>,
}

#[async_trait]
impl CredentialStore for BoundStore {
    async fn begin(
        &self,
        key: &CredentialKey,
    ) -> std::result::Result<Box<dyn CredentialTransaction>, StoreError> {
        if key.provider != self.mechanism {
            return Err(StoreError::Conflict);
        }
        let slot = self
            .accounts
            .get(&key.account)
            .ok_or(StoreError::Conflict)?;
        self.credentials.begin(slot).await
    }
}

struct SharedApplier {
    mechanism: String,
    inner: Arc<dyn AuthApplier>,
}

impl SharedApplier {
    fn target(&self, target: &ModelTarget) -> Result<ModelTarget> {
        if !target.api_key.is_empty() {
            return Err(ModelError::invalid_credential(
                "shared authentication forbids inline credentials",
            ));
        }
        let mut target = target.clone();
        target.provider_name.clone_from(&self.mechanism);
        Ok(target)
    }
}

#[async_trait]
impl AuthApplier for SharedApplier {
    fn requires_streaming(&self, target: &ModelTarget) -> bool {
        self.target(target)
            .is_ok_and(|target| self.inner.requires_streaming(&target))
    }
    fn output_token_limit_support(&self, target: &ModelTarget) -> Option<bool> {
        self.target(target)
            .ok()
            .and_then(|target| self.inner.output_token_limit_support(&target))
    }
    fn normalize_managed_body(
        &self,
        body: &mut serde_json::Value,
        target: &ModelTarget,
    ) -> Result<()> {
        self.inner
            .normalize_managed_body(body, &self.target(target)?)
    }
    async fn prepare_body(&self, body: &mut serde_json::Value, target: &ModelTarget) -> Result<()> {
        // All assembled mechanisms shape from the protocol/target, not from a
        // credential read. Their apply result binds the actual account version.
        self.inner.prepare_body(body, &self.target(target)?).await
    }
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
        request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<AppliedAuth> {
        let observation = Observation::default();
        let applied = observation
            .scope(
                self.inner
                    .apply_with_authority(request, &self.target(target)?),
            )
            .await?;
        let stamp = observation.finish().map_err(storage_error)?;
        Ok(applied.bind_shared_credential(stamp))
    }
    async fn continuation_authority(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        self.inner
            .continuation_authority(&self.target(target)?)
            .await
    }
    async fn continuation_authority_proof(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<ContinuationAuthority>> {
        self.inner
            .continuation_authority_proof(&self.target(target)?)
            .await
    }
    async fn refresh_after_unauthorized(
        &self,
        target: &ModelTarget,
        rejected: Option<&reqwest::header::HeaderValue>,
    ) -> Result<bool> {
        self.inner
            .refresh_after_unauthorized(&self.target(target)?, rejected)
            .await
    }
}

struct StaticKey {
    store: Arc<dyn CredentialStore>,
}

impl StaticKey {
    async fn resolve(&self, target: &ModelTarget) -> Result<String> {
        let mut transaction = self
            .store
            .begin(&CredentialKey {
                provider: target.provider_name.clone(),
                account: target
                    .account_label
                    .as_deref()
                    .unwrap_or(DEFAULT_ACCOUNT)
                    .into(),
            })
            .await
            .map_err(storage_error)?;
        let key = match transaction.credential() {
            Some(Credential::ApiKey { value }) if !value.is_empty() => Some(value.clone()),
            _ => None,
        };
        transaction.release().await.map_err(storage_error)?;
        key.ok_or_else(|| {
            ModelError::invalid_credential("shared provider requires a stored API key")
        })
    }
}

#[async_trait]
impl AuthApplier for StaticKey {
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
        request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<AppliedAuth> {
        let mut target = target.clone();
        target.api_key = self.resolve(&target).await?;
        let transport: &dyn Transport = match &target.api_protocol {
            ApiProtocol::ChatCompletions => &protocol::chat_completions::ChatCompletionsTransport,
            ApiProtocol::Messages => &protocol::messages::MessagesTransport,
            ApiProtocol::Responses => &protocol::responses::ResponsesTransport,
            ApiProtocol::Decisions => &protocol::decisions::DecisionsTransport,
            ApiProtocol::Custom(_) => {
                return Err(ModelError::configuration(
                    "shared authentication requires a built-in protocol",
                ));
            }
        };
        let request = transport.authorise(request, &target).await?;
        Ok(AppliedAuth::proven(
            request,
            CredentialAuthority::derive("static-transport-credential", &target.api_key),
        ))
    }
    async fn continuation_authority(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        Ok(Some(CredentialAuthority::derive(
            "static-transport-credential",
            &self.resolve(target).await?,
        )))
    }
}

fn storage_error(failure: StoreError) -> ModelError {
    ModelError::CredentialStorage { failure }
}
