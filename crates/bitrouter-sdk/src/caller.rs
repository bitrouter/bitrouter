//! Caller identity.
//!
//! [`CallerContext`] identifies the authenticated client of a request. Every
//! protocol's `*PipelineContext` embeds one. The SDK keeps this minimal — it
//! stores opaque identity (`api_key_id`, `user_id`) and a `local` flag for
//! the `server.skip_auth` path. Anything richer (payment method, plan tier,
//! org id, etc.) is deployment-specific and lives in the binary's business
//! modules — pipeline-level code does not need to interpret it.
//!
//! Hooks set or upgrade the caller during the pre-request stage — typically
//! an `AuthHook` resolves a credential into a known caller. When
//! `server.skip_auth` is set, a credential-less request is given the
//! synthetic [`CallerContext::local`] caller.

use serde::{Deserialize, Serialize};

/// The authenticated (or synthesised) caller of a request.
///
/// Populated at Stage 0. Read-only for the rest of the pipeline. When
/// `server.skip_auth` is on and a request carries no credentials, the SDK
/// synthesises a local `CallerContext`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallerContext {
    api_key_id: String,
    user_id: String,
    /// True when synthesised by `server.skip_auth` rather than authenticated.
    local: bool,
}

impl CallerContext {
    /// Construct a caller context from authenticated identity.
    pub fn new(api_key_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            api_key_id: api_key_id.into(),
            user_id: user_id.into(),
            local: false,
        }
    }

    /// The synthesised local caller used when `server.skip_auth` is on.
    pub fn local() -> Self {
        Self {
            api_key_id: "local".to_string(),
            user_id: "local".to_string(),
            local: true,
        }
    }

    /// A pre-auth placeholder caller. Used when `skip_auth` is off — a Stage-1
    /// `PreRequestHook` is expected to validate credentials and replace it via
    /// [`crate::language_model::PipelineContext::set_caller`] (LLM pipeline) or
    /// [`crate::mcp::McpContext::set_caller`] (MCP pipeline). If no hook
    /// upgrades it, downstream stages see an anonymous caller.
    pub fn anonymous() -> Self {
        Self {
            api_key_id: "anonymous".to_string(),
            user_id: "anonymous".to_string(),
            local: false,
        }
    }

    /// Whether this is the pre-auth anonymous placeholder.
    pub fn is_anonymous(&self) -> bool {
        !self.local && self.api_key_id == "anonymous"
    }

    /// The API key id this caller authenticated with.
    pub fn api_key_id(&self) -> &str {
        &self.api_key_id
    }

    /// The owning user id.
    pub fn user_id(&self) -> &str {
        &self.user_id
    }

    /// Whether this caller was synthesised locally (`skip_auth`).
    pub fn is_local(&self) -> bool {
        self.local
    }
}
