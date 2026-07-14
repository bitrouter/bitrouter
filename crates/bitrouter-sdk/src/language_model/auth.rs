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

use crate::error::Result;
use crate::language_model::types::RoutingTarget;

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
}
