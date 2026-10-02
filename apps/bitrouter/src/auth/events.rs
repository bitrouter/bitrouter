//! Pipeline events emitted by the OSS auth module.
//!
//! Downstream hooks (the binary's `policy` and `metering` modules) read
//! these events for caller identity instead of querying the `api_keys`
//! table directly.

use serde::Serialize;

use bitrouter_sdk::PipelineEvent;

/// Authentication succeeded — carries the caller's identity. Downstream
/// hooks take identity from this event, not from the `api_keys` table.
#[derive(Debug, Clone, Serialize)]
pub struct Authenticated {
    /// The authenticated api key id.
    pub api_key_id: String,
    /// The owning user id.
    pub user_id: String,
    /// The policy id bound to the key, if any (read by `crate::policy`).
    pub policy_id: Option<String>,
}

impl PipelineEvent for Authenticated {
    fn event_name(&self) -> &'static str {
        "auth.authenticated"
    }
}

/// Opaque route namespace derived from the ordinary API credential.
///
/// This value is internal correlation state, not a second credential. The
/// public API-key id remains on [`Authenticated`] and the request caller.
#[derive(Debug, Clone, Serialize)]
pub struct ApiPrincipalEstablished {
    /// Stable per-credential route namespace.
    pub route_scope_id: String,
}

impl PipelineEvent for ApiPrincipalEstablished {
    fn event_name(&self) -> &'static str {
        "auth.api_principal_established"
    }
}
