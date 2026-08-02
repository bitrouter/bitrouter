//! Per-provider authentication overrides for outbound HTTP requests.
//!
//! By default the per-protocol [`Transport`](super::protocol::Transport)
//! applies the credential header it expects (OpenAI Bearer, Anthropic
//! `x-api-key`, Google `x-goog-api-key`). For providers whose credential
//! flow is more involved — OAuth with a separate token-exchange step,
//! AWS SigV4, anything stateful — register an [`AuthApplier`] keyed by
//! provider id on the [`HttpExecutor`](super::HttpExecutor). When the
//! executor finds a matching applier it routes through it **instead of**
//! `Transport::authorise`.
//!
//! Implementations live in their own crates (e.g. `bitrouter-providers`
//! ships the GitHub Copilot applier).

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::error::Result;
use crate::language_model::types::RoutingTarget;

const AUTHORITY_DOMAIN: &[u8] = b"bitrouter.transport.credential-authority.v1";

/// Redaction-safe stable identity for the credential principal used on one
/// outbound request. The value is already a one-way digest; raw credentials
/// and account identifiers never enter pipeline events, logs, or storage.
#[derive(Clone, PartialEq, Eq)]
pub struct CredentialAuthority([u8; 32]);

impl CredentialAuthority {
    /// Derive an authority proof from a provider-scoped stable identity.
    ///
    /// `identity` may be a stable account/subject id or, when no stable
    /// principal exists, the long-lived stored credential itself. Callers must
    /// never log the input or retain it beyond this constructor.
    pub fn derive(namespace: &str, identity: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(AUTHORITY_DOMAIN);
        digest.update((namespace.len() as u64).to_be_bytes());
        digest.update(namespace.as_bytes());
        digest.update((identity.len() as u64).to_be_bytes());
        digest.update(identity.as_bytes());
        Self(digest.finalize().into())
    }

    /// Derive a proof from an additional provider-controlled identity scope,
    /// such as an OAuth issuer. Every component is length-delimited so
    /// distinct `(scope, identity)` pairs cannot collide by concatenation.
    pub fn derive_scoped(namespace: &str, scope: &str, identity: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(AUTHORITY_DOMAIN);
        for component in [namespace, scope, identity] {
            digest.update((component.len() as u64).to_be_bytes());
            digest.update(component.as_bytes());
        }
        Self(digest.finalize().into())
    }

    /// Digest bytes for a second, installation-keyed fingerprinting layer.
    pub fn proof_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for CredentialAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialAuthority(<redacted>)")
    }
}

/// Result of applying authentication to one exact outbound request.
///
/// Dynamic appliers return the stable authority proof atomically with the
/// request whose transport credential they just installed. `None` means the
/// applier cannot prove a stable continuation authority; ordinary requests
/// still work, but successful native Responses continuation publication fails
/// closed.
pub struct AppliedAuth {
    request: reqwest::Request,
    continuation_authority: Option<CredentialAuthority>,
}

impl AppliedAuth {
    /// Build an authenticated request with a proven stable authority.
    pub fn proven(request: reqwest::Request, authority: CredentialAuthority) -> Self {
        Self {
            request,
            continuation_authority: Some(authority),
        }
    }

    /// Build an authenticated request whose applier cannot prove a stable
    /// continuation authority.
    pub fn unproven(request: reqwest::Request) -> Self {
        Self {
            request,
            continuation_authority: None,
        }
    }

    /// Discard continuation proof metadata and recover the authenticated
    /// request. This supports the source-compatible legacy
    /// [`AuthApplier::apply`] entry point for authority-aware built-ins.
    pub fn into_request(self) -> reqwest::Request {
        self.request
    }

    pub(crate) fn into_parts(self) -> (reqwest::Request, Option<CredentialAuthority>) {
        (self.request, self.continuation_authority)
    }
}

impl std::ops::Deref for AppliedAuth {
    type Target = reqwest::Request;

    fn deref(&self) -> &Self::Target {
        &self.request
    }
}

impl std::ops::DerefMut for AppliedAuth {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.request
    }
}

impl std::fmt::Debug for AppliedAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppliedAuth")
            .field("method", self.request.method())
            .field("url", self.request.url())
            .field(
                "continuation_authority_proven",
                &self.continuation_authority.is_some(),
            )
            .finish()
    }
}

/// Apply provider-specific authentication to a built `reqwest::Request`.
///
/// Receives ownership of the request and the resolved [`RoutingTarget`];
/// returns the request with credentials + any required integration headers
/// added. May perform async work (token-store reads, network token
/// exchanges) — the executor awaits the result before sending.
#[async_trait]
pub trait AuthApplier: Send + Sync {
    /// Apply authentication. The default `Transport::authorise` is **not**
    /// called when this applier runs; the applier owns the full credential
    /// surface for the request.
    async fn apply(
        &self,
        request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request>;

    /// Apply authentication and atomically report the stable continuation
    /// authority used by that exact request.
    ///
    /// The default delegates to the legacy [`apply`](Self::apply) contract and
    /// returns an unproven authority. This preserves source compatibility for
    /// existing out-of-tree appliers and keeps their ordinary requests
    /// working, while native Responses continuation remains fail-closed until
    /// an applier explicitly opts into stable authority proof.
    async fn apply_with_authority(
        &self,
        request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<AppliedAuth> {
        Ok(AppliedAuth::unproven(self.apply(request, target).await?))
    }

    /// Resolve the stable authority that a newly authenticated request would
    /// use. Continuation route matching calls this before upstream dispatch.
    /// The later [`apply_with_authority`](Self::apply_with_authority) result
    /// remains authoritative for the exact request and is checked against this
    /// proof before send.
    async fn continuation_authority(
        &self,
        _target: &RoutingTarget,
    ) -> Result<Option<CredentialAuthority>> {
        Ok(None)
    }

    /// Optionally rewrite the structured request body before it is
    /// serialized and sent. Runs at render time — after the protocol
    /// adapter produces the JSON body and before the HTTP request is built,
    /// so it sees the body as a mutable [`serde_json::Value`]. This is the
    /// right layer for body edits; [`apply`](Self::apply) only sees an
    /// already-built request whose body is opaque bytes.
    ///
    /// The default is a no-op. OAuth *subscription* providers override it to
    /// match the body shape the vendor's own first-party client sends — for
    /// example Claude Pro/Max requires Claude Code's identity as the first
    /// `system` block, and the ChatGPT/Codex backend requires `store: false`
    /// on the Responses body. Static-credential providers never need it.
    async fn prepare_body(
        &self,
        _body: &mut serde_json::Value,
        _target: &RoutingTarget,
    ) -> Result<()> {
        Ok(())
    }

    /// Give stateful auth providers one chance to recover from an upstream
    /// `401 Unauthorized`.
    ///
    /// The executor calls this only after the upstream rejects an already
    /// authenticated request. Implementations should refresh or reload their
    /// credential state, then return `true` when the request should be rebuilt
    /// and retried once. Static-credential providers keep the default `false`
    /// and preserve the original upstream error.
    async fn refresh_after_unauthorized(
        &self,
        _target: &RoutingTarget,
        _rejected_authorization: Option<&reqwest::header::HeaderValue>,
    ) -> Result<bool> {
        Ok(false)
    }
}

/// Registry of per-provider [`AuthApplier`]s, keyed by `provider_name`.
///
/// Empty by default — the executor falls through to `Transport::authorise`
/// for any provider with no registered applier, which is the right path for
/// static-credential providers.
#[derive(Default, Clone)]
pub struct AuthAppliers {
    by_provider: HashMap<String, Arc<dyn AuthApplier>>,
}

impl AuthAppliers {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `applier` for requests whose `target.provider_name == provider_id`.
    /// Re-registering overwrites the previous entry.
    pub fn register(&mut self, provider_id: impl Into<String>, applier: Arc<dyn AuthApplier>) {
        self.by_provider.insert(provider_id.into(), applier);
    }

    /// Chained-builder form of [`register`](Self::register).
    pub fn with(mut self, provider_id: impl Into<String>, applier: Arc<dyn AuthApplier>) -> Self {
        self.register(provider_id, applier);
        self
    }

    /// Look up an applier for `provider_id`.
    pub fn lookup(&self, provider_id: &str) -> Option<&Arc<dyn AuthApplier>> {
        self.by_provider.get(provider_id)
    }

    /// Whether any appliers are registered.
    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }

    /// Resolve a target's stable continuation authority. Registered dynamic
    /// appliers own the proof; static transport auth derives it from the
    /// effective configured credential without retaining the secret.
    pub async fn continuation_authority(
        &self,
        target: &RoutingTarget,
    ) -> Result<Option<CredentialAuthority>> {
        if let Some(applier) = self.lookup(&target.provider_name) {
            return applier.continuation_authority(target).await;
        }
        let credential = target
            .api_key_override
            .as_deref()
            .unwrap_or(target.api_key.as_str());
        Ok(Some(CredentialAuthority::derive(
            "static-transport-credential",
            credential,
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::types::ApiProtocol;

    struct LegacyApplier;

    #[async_trait]
    impl AuthApplier for LegacyApplier {
        async fn apply(
            &self,
            mut request: reqwest::Request,
            _target: &RoutingTarget,
        ) -> Result<reqwest::Request> {
            request.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_static("Bearer legacy"),
            );
            Ok(request)
        }
    }

    fn target() -> RoutingTarget {
        RoutingTarget {
            provider_name: "legacy-provider".into(),
            service_id: "legacy-model".into(),
            api_base: "https://example.invalid".into(),
            api_key: String::new(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    #[tokio::test]
    async fn legacy_applier_remains_compatible_and_is_conservatively_unproven() {
        let request = reqwest::Client::new()
            .post("https://example.invalid")
            .build()
            .unwrap();

        let applied = LegacyApplier
            .apply_with_authority(request, &target())
            .await
            .unwrap();

        assert_eq!(
            applied.request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer legacy"
        );
        assert!(applied.continuation_authority.is_none());
    }
}
