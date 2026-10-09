//! One selected model target, after the caller resolves routing and accounts.

use crate::types::{ApiProtocol, AuthScheme, ModelCompatibility};

/// Provenance of a credential value already supplied by the caller.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum CredentialPriority {
    /// An explicit call override; use it before any stored credential.
    #[default]
    Explicit,
    /// Application-permitted fallback; use it only when the selected slot is absent.
    Fallback,
}

/// Effective connection and wire compatibility for a single model call.
///
/// The caller selects credentials and resolves any per-request overrides before
/// constructing this value. An optional account label names the caller-selected
/// slot; it carries no routing or account-selection policy.
#[derive(Clone)]
pub struct ModelTarget {
    /// Provider identifier used in diagnostics.
    pub provider_name: String,
    /// Native model identifier used by the provider endpoint.
    pub service_id: String,
    /// Selected provider wire protocol.
    pub api_protocol: ApiProtocol,
    /// Effective provider API base URL.
    pub api_base: String,
    /// Effective credential supplied by the caller.
    pub api_key: String,
    /// Whether this value overrides storage or is permitted only when absent.
    pub credential_priority: CredentialPriority,
    /// Explicit account slot for registered authentication mechanisms.
    pub account_label: Option<String>,
    /// How the selected transport presents the credential.
    pub auth_scheme: AuthScheme,
    /// Provider/model wire spelling and optional-field support.
    pub compatibility: ModelCompatibility,
}

impl std::fmt::Debug for ModelTarget {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ModelTarget")
            .field("provider_name", &self.provider_name)
            .field("service_id", &self.service_id)
            .field("api_protocol", &self.api_protocol)
            .field("api_base", &self.api_base)
            .field("api_key", &"<redacted>")
            .field("credential_priority", &self.credential_priority)
            .field("account_selected", &self.account_label.is_some())
            .field("auth_scheme", &self.auth_scheme)
            .field("compatibility", &self.compatibility)
            .finish()
    }
}

impl ModelTarget {
    /// Non-empty explicit call credential, without reading any other source.
    pub fn explicit_credential(&self) -> Option<&str> {
        (self.credential_priority == CredentialPriority::Explicit && !self.api_key.is_empty())
            .then_some(self.api_key.as_str())
    }
    /// Non-empty fallback value already resolved and permitted by the application.
    pub fn fallback_credential(&self) -> Option<&str> {
        (self.credential_priority == CredentialPriority::Fallback && !self.api_key.is_empty())
            .then_some(self.api_key.as_str())
    }
}
