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
    /// Which `bitrouter launch` session this request belongs to, when one
    /// minted the credential it arrived with. See [`launch_tag`].
    #[serde(default, skip_serializing_if = "Option::is_none")]
    launch_id: Option<String>,
    /// Private authorization/continuation scope when it must be narrower than
    /// the low-cardinality user exposed to metering and aggregate telemetry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    security_scope_id: Option<String>,
}

/// Prefix of the opaque per-launch attribution token `bitrouter launch` mints
/// when it — and not the user — owns the gateway credential slot.
pub const LAUNCH_TOKEN_PREFIX: &str = "brl_";

/// Read a launch tag out of a bearer credential.
///
/// **This is attribution, not authentication.** The tag is accepted only on
/// the `skip_auth` path, where the daemon already serves every local caller
/// without credentials — so a client that could forge a tag could equally
/// well just spend. It buys the ability to say *which launch* spent what on a
/// zero-config install; it grants nothing, and no authorization decision may
/// ever read it.
///
/// Returns `None` for an absent, non-bearer, or non-launch credential, so a
/// real key is never mistaken for a tag.
pub fn launch_tag(authorization: Option<&str>) -> Option<String> {
    let header = authorization?.trim();
    // The scheme is case-insensitive per RFC 7235; harnesses differ on it.
    let (scheme, token) = header.split_once(' ')?;
    if !scheme.eq_ignore_ascii_case("bearer") {
        return None;
    }
    let token = token.trim();
    // A bare prefix with no body is not an identifier.
    if token.len() <= LAUNCH_TOKEN_PREFIX.len() || !token.starts_with(LAUNCH_TOKEN_PREFIX) {
        return None;
    }
    Some(token.to_string())
}

impl CallerContext {
    /// Construct a caller context from authenticated identity.
    pub fn new(api_key_id: impl Into<String>, user_id: impl Into<String>) -> Self {
        Self {
            api_key_id: api_key_id.into(),
            user_id: user_id.into(),
            local: false,
            launch_id: None,
            security_scope_id: None,
        }
    }

    /// Construct an authenticated caller with a private ownership scope.
    pub fn new_scoped(
        api_key_id: impl Into<String>,
        user_id: impl Into<String>,
        security_scope_id: impl Into<String>,
    ) -> Self {
        Self {
            api_key_id: api_key_id.into(),
            user_id: user_id.into(),
            local: false,
            launch_id: None,
            security_scope_id: Some(security_scope_id.into()),
        }
    }

    /// The synthesised local caller used when `server.skip_auth` is on.
    pub fn local() -> Self {
        Self {
            api_key_id: "local".to_string(),
            user_id: "local".to_string(),
            local: true,
            launch_id: None,
            security_scope_id: None,
        }
    }

    /// The local caller, tagged with the `bitrouter launch` session whose
    /// minted credential this request arrived with.
    ///
    /// The identity is unchanged — still `local`, still unauthenticated. Only
    /// the attribution dimension is added, which is what lets two concurrent
    /// launches on a default `skip_auth: true` install report separate spend
    /// without anyone turning auth on to find out what their agent cost.
    pub fn local_launch(launch_id: impl Into<String>) -> Self {
        Self {
            launch_id: Some(launch_id.into()),
            ..Self::local()
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
            launch_id: None,
            security_scope_id: None,
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

    /// The `bitrouter launch` session this request belongs to, if any.
    pub fn launch_id(&self) -> Option<&str> {
        self.launch_id.as_deref()
    }

    /// Security ownership scope used by durable continuation state. Ordinary
    /// callers retain their existing user-id scope.
    pub fn security_scope_id(&self) -> &str {
        self.security_scope_id.as_deref().unwrap_or(&self.user_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_launch_token_is_recognized_regardless_of_scheme_case() {
        assert_eq!(
            launch_tag(Some("Bearer brl_01J8XYZ")).as_deref(),
            Some("brl_01J8XYZ")
        );
        assert_eq!(
            launch_tag(Some("bearer brl_01J8XYZ")).as_deref(),
            Some("brl_01J8XYZ")
        );
    }

    #[test]
    fn a_real_key_is_never_mistaken_for_a_launch_tag() {
        // The whole design rests on this: attribution must never change how a
        // credential is treated, so anything that is not our minted token has
        // to fall through untouched.
        assert_eq!(launch_tag(Some("Bearer brk_realkey")), None);
        assert_eq!(launch_tag(Some("Bearer sk-ant-whatever")), None);
        assert_eq!(launch_tag(Some("Bearer bitrouter-local")), None);
        assert_eq!(launch_tag(Some("Basic brl_01J8XYZ")), None);
        assert_eq!(launch_tag(Some("brl_01J8XYZ")), None, "scheme required");
        assert_eq!(launch_tag(None), None);
    }

    #[test]
    fn a_bare_prefix_is_not_an_identifier() {
        // Otherwise every launch that somehow sent an empty tag would collapse
        // into one shared bucket, which is worse than no attribution.
        assert_eq!(launch_tag(Some("Bearer brl_")), None);
        assert_eq!(launch_tag(Some("Bearer ")), None);
    }

    #[test]
    fn tagging_adds_attribution_without_changing_identity() {
        let plain = CallerContext::local();
        let tagged = CallerContext::local_launch("brl_abc");
        assert_eq!(tagged.api_key_id(), plain.api_key_id());
        assert_eq!(tagged.user_id(), plain.user_id());
        assert!(tagged.is_local(), "still an unauthenticated local caller");
        assert_eq!(tagged.launch_id(), Some("brl_abc"));
        assert_eq!(plain.launch_id(), None);
    }
}
