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
