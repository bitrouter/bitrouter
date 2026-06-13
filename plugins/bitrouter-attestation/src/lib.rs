//! # bitrouter-attestation-plugin
//!
//! A [`Plugin`] that installs an [`AttestationRouteHook`]: at route-resolution
//! it looks up a TEE-attestation verdict (via a [`VerifierRegistry`]) for each
//! confidential routing target and either **records** it (default — tags the
//! request context, never drops a target) or **enforces** it (drops unverified
//! targets, fail-closed routing). See the refactor spec §7, Decision 1.
//!
//! The plugin holds only a verifier handle + policy; all crypto lives in the
//! pure `bitrouter-attestation` crate.

#![forbid(unsafe_code)]

mod hooks;

#[cfg(test)]
mod tests;

use std::sync::Arc;

use bitrouter_attestation::VerifierRegistry;
use bitrouter_sdk::{AppBuilder, Plugin, PluginId};

pub use hooks::{AttestationOutcome, AttestationRouteHook};

/// How the hook treats an unverified confidential target.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Default)]
pub enum AttestationPolicy {
    /// Tag the request with the verdict; never drop a target (spec Decision 1).
    /// A verification hiccup can't fail a user's chat.
    #[default]
    Record,
    /// Drop targets whose verdict isn't `verified` — fail-closed routing.
    Enforce,
}

impl AttestationPolicy {
    pub fn label(self) -> &'static str {
        match self {
            Self::Record => "record",
            Self::Enforce => "enforce",
        }
    }
}

/// Plugin configuration: the policy, the verifier registry, and which providers
/// are treated as confidential (default `["near-ai"]`).
#[derive(Clone)]
pub struct AttestationConfig {
    pub policy: AttestationPolicy,
    pub registry: Arc<VerifierRegistry>,
    pub confidential_providers: Vec<String>,
}

impl AttestationConfig {
    /// Build a config defaulting `confidential_providers` to `["near-ai"]`.
    pub fn new(policy: AttestationPolicy, registry: Arc<VerifierRegistry>) -> Self {
        Self {
            policy,
            registry,
            confidential_providers: vec!["near-ai".to_string()],
        }
    }

    /// Override the set of providers treated as confidential.
    pub fn with_confidential_providers(mut self, providers: Vec<String>) -> Self {
        self.confidential_providers = providers;
        self
    }

    fn is_confidential(&self, provider: &str) -> bool {
        self.confidential_providers.iter().any(|p| p == provider)
    }
}

/// The attestation [`Plugin`]. Registers a single route hook.
pub struct AttestationPlugin {
    id: PluginId,
    config: AttestationConfig,
}

impl AttestationPlugin {
    pub fn new(config: AttestationConfig) -> Self {
        Self {
            id: PluginId::new("bitrouter-attestation"),
            config,
        }
    }
}

impl Plugin for AttestationPlugin {
    fn id(&self) -> &PluginId {
        &self.id
    }

    fn install(&self, app: &mut AppBuilder) {
        app.language_model_builder()
            .route_hook(AttestationRouteHook::new(self.config.clone()));
    }
}
