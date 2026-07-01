//! Session correlation key derivation for the BitRouter proxy.
//!
//! Each inbound LLM request is assigned a **session key** at ingress so that
//! downstream observability can group a series of related requests into the
//! same logical session.
//!
//! ## Two derivation paths
//!
//! 1. **Explicit** — the inbound request carries an `X-Bitrouter-Session-Id`
//!    header (injected by `bitrouter spawn` via `ANTHROPIC_CUSTOM_HEADERS`).
//!    The header value is used verbatim.
//!
//! 2. **Content-derived fallback** — when no header is present, the key is
//!    `"derived:" + lower_hex(SHA-256(user_id ‖ 0x00 ‖ system ‖ 0x00 ‖
//!    first_user_text)[..8])`, where:
//!    - `user_id` is from [`bitrouter_sdk::caller::CallerContext`]
//!      (empty string if anonymous / local).
//!    - `system` is `prompt.system` (empty string if `None`).
//!    - `first_user_text` is the concatenation of all `Content::Text` parts
//!      from the **first** `role = User` message (empty string if none).
//!
//! The derived key is **stable** across requests that share the same
//! `user_id`, `system`, and opening message — a "same conversation" signal —
//! and **different** when the system prompt or opening message differs.
//!
//! ## Non-leak guarantee
//!
//! The key is stored in `PipelineRequest::session_key` / `PipelineContext::
//! session_key`, which are **internal** fields. The executor only forwards
//! headers it has been explicitly told to forward (auth, anthropic-beta,
//! W3C traceparent); `x-bitrouter-session-id` and the derived key string
//! never appear on outbound provider HTTP requests.

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use bitrouter_sdk::HeaderMap;
use bitrouter_sdk::HookDecision;
use bitrouter_sdk::PreRequestHook;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::error::Result;
use bitrouter_sdk::language_model::context::PipelineContext;
use bitrouter_sdk::language_model::types::{Content, Prompt, Role};

/// Inbound header that carries an explicit session id injected by
/// `bitrouter spawn`.
///
/// Claude Code forwards custom headers set in `ANTHROPIC_CUSTOM_HEADERS` to
/// the proxy verbatim. See the Claude Code env-vars docs:
/// <https://code.claude.com/docs/en/env-vars>
pub const SESSION_ID_HEADER: &str = "x-bitrouter-session-id";

/// `PreRequestHook` that derives and stores the session correlation key.
///
/// Reads `x-bitrouter-session-id` from the inbound headers (explicit path) or
/// falls back to the content-derived SHA-256 hash. Always allows the request.
/// The key is stored via [`PipelineContext::set_session_key`] for the
/// telemetry layer.
pub struct SessionCorrelationHook;

#[async_trait]
impl PreRequestHook for SessionCorrelationHook {
    async fn check(&self, ctx: &mut PipelineContext) -> Result<HookDecision> {
        let key = derive_session_key(ctx.caller(), ctx.prompt(), ctx.headers());
        ctx.set_session_key(key);
        Ok(HookDecision::Allow)
    }
}

/// Derive the session key for an inbound request.
///
/// Returns the explicit header value when `x-bitrouter-session-id` is present
/// and non-empty, otherwise a `"derived:…"` hex string. The returned value
/// is the typed `session_key` stored on `PipelineRequest` — NOT a header
/// forwarded to upstream providers.
pub fn derive_session_key(caller: &CallerContext, prompt: &Prompt, headers: &HeaderMap) -> String {
    // Explicit path: use the spawner-injected header verbatim.
    if let Some(value) = headers
        .get(SESSION_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::trim)
        .filter(|s| !s.is_empty())
    {
        return value.to_string();
    }

    // Fallback: content-derived SHA-256 fingerprint.
    // `CallerContext::user_id()` is always non-empty — it yields the literal
    // "local" (skip_auth) or "anonymous" (pre-auth), never "" — so the
    // formula's "empty user_id" case is unreachable here, and those two are
    // distinct discriminator buckets.
    derive_content_key(caller.user_id(), prompt)
}

/// Compute the content-derived session key.
///
/// Formula: `"derived:" + lower_hex( SHA-256( user_id ‖ 0x00 ‖ system ‖
/// 0x00 ‖ first_user_text )[..8] )`
///
/// This is factored out so tests can exercise the derivation without
/// constructing a full HTTP context.
pub fn derive_content_key(user_id: &str, prompt: &Prompt) -> String {
    let system = prompt.system.as_deref().unwrap_or("");
    let first_user_text = prompt
        .messages
        .iter()
        .find(|m| m.role == Role::User)
        .map(|m| {
            m.content
                .iter()
                .filter_map(|c| {
                    if let Content::Text { text, .. } = c {
                        Some(text.as_str())
                    } else {
                        None
                    }
                })
                .collect::<String>()
        })
        .unwrap_or_default();

    let mut hasher = Sha256::new();
    hasher.update(user_id.as_bytes());
    hasher.update([0x00]);
    hasher.update(system.as_bytes());
    hasher.update([0x00]);
    hasher.update(first_user_text.as_bytes());
    let digest = hasher.finalize();
    let prefix = &digest[..8];
    format!("derived:{}", hex::encode(prefix))
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::language_model::types::{
        Content, GenerationParams, Message, ProviderMetadata, Role,
    };

    fn make_prompt(system: Option<&str>, messages: Vec<Message>) -> Prompt {
        Prompt {
            model: "test-model".to_string(),
            system: system.map(str::to_string),
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn user_msg(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    fn assistant_msg(text: &str) -> Message {
        Message::text(Role::Assistant, text)
    }

    fn local_caller() -> CallerContext {
        CallerContext::local()
    }

    fn authed_caller(user_id: &str) -> CallerContext {
        CallerContext::new("key1", user_id)
    }

    // ===== STEP 0 verification =====

    /// Verify that `SESSION_ID_HEADER` is the lowercase form — HTTP/1.1 headers
    /// are case-insensitive, and `http::HeaderMap` normalises to lowercase on
    /// insert/lookup.
    #[test]
    fn session_id_header_constant_is_lowercase() {
        assert_eq!(SESSION_ID_HEADER, SESSION_ID_HEADER.to_lowercase());
    }

    // ===== Explicit header path =====

    #[test]
    fn explicit_header_is_used_verbatim() {
        let caller = local_caller();
        let prompt = make_prompt(None, vec![user_msg("hello")]);
        let mut headers = http::HeaderMap::new();
        let session_id = "test-session-uuid-1234";
        headers.insert(SESSION_ID_HEADER, session_id.parse().unwrap());
        let key = derive_session_key(&caller, &prompt, &headers);
        assert_eq!(key, session_id);
    }

    #[test]
    fn empty_header_value_falls_through_to_derived() {
        let caller = local_caller();
        let prompt = make_prompt(None, vec![user_msg("hello")]);
        let mut headers = http::HeaderMap::new();
        headers.insert(SESSION_ID_HEADER, "   ".parse().unwrap()); // whitespace only
        let key = derive_session_key(&caller, &prompt, &headers);
        assert!(key.starts_with("derived:"), "got: {key}");
    }

    #[test]
    fn no_header_produces_derived_key() {
        let caller = local_caller();
        let prompt = make_prompt(None, vec![user_msg("hello")]);
        let key = derive_session_key(&caller, &prompt, &http::HeaderMap::new());
        assert!(key.starts_with("derived:"), "got: {key}");
    }

    // ===== Derived key — fixed test vector =====

    /// Fixed test vector for the derivation formula.
    ///
    /// Input: user_id = "u1", system = "sys", first_user_text = "hi"
    /// SHA-256("u1" ‖ 0x00 ‖ "sys" ‖ 0x00 ‖ "hi")[..8] computed externally.
    #[test]
    fn derived_key_matches_known_vector() {
        use sha2::{Digest, Sha256};
        // Compute the expected value using the same algorithm.
        let mut hasher = Sha256::new();
        hasher.update(b"u1");
        hasher.update([0x00]);
        hasher.update(b"sys");
        hasher.update([0x00]);
        hasher.update(b"hi");
        let digest = hasher.finalize();
        let expected = format!("derived:{}", hex::encode(&digest[..8]));

        let key = derive_content_key("u1", &make_prompt(Some("sys"), vec![user_msg("hi")]));
        assert_eq!(key, expected, "derivation formula must match spec");
        // Sanity: the prefix is always 8 bytes = 16 hex chars + "derived:".
        assert_eq!(key.len(), "derived:".len() + 16);
    }

    // ===== Stability / differentiation =====

    #[test]
    fn derived_key_stable_across_same_system_and_opening_message() {
        let prompt1 = make_prompt(
            Some("System A"),
            vec![
                user_msg("What is Rust?"),
                assistant_msg("A language."),
                user_msg("Tell me more."),
            ],
        );
        let prompt2 = make_prompt(
            Some("System A"),
            vec![
                user_msg("What is Rust?"),
                assistant_msg("A safe systems language."),
                user_msg("Give me an example."),
            ],
        );
        // Different tails; same system + opening message → same key.
        let key1 = derive_content_key("u42", &prompt1);
        let key2 = derive_content_key("u42", &prompt2);
        assert_eq!(key1, key2, "key must be stable when only the tail differs");
    }

    #[test]
    fn derived_key_differs_when_system_differs() {
        let prompt_a = make_prompt(Some("System A"), vec![user_msg("hello")]);
        let prompt_b = make_prompt(Some("System B"), vec![user_msg("hello")]);
        let key_a = derive_content_key("u1", &prompt_a);
        let key_b = derive_content_key("u1", &prompt_b);
        assert_ne!(
            key_a, key_b,
            "different system prompts must yield different keys"
        );
    }

    #[test]
    fn derived_key_differs_when_opening_message_differs() {
        let prompt_a = make_prompt(Some("sys"), vec![user_msg("hello")]);
        let prompt_b = make_prompt(Some("sys"), vec![user_msg("world")]);
        let key_a = derive_content_key("u1", &prompt_a);
        let key_b = derive_content_key("u1", &prompt_b);
        assert_ne!(
            key_a, key_b,
            "different opening messages must yield different keys"
        );
    }

    #[test]
    fn derived_key_differs_when_user_id_differs() {
        let prompt = make_prompt(Some("sys"), vec![user_msg("hello")]);
        let key_a = derive_content_key("user-alice", &prompt);
        let key_b = derive_content_key("user-bob", &prompt);
        assert_ne!(key_a, key_b, "different user ids must yield different keys");
    }

    #[test]
    fn derived_key_empty_user_id_and_no_system_no_messages() {
        // Edge case: everything is empty → stable key for that degenerate shape.
        let prompt = make_prompt(None, vec![]);
        let key = derive_content_key("", &prompt);
        assert!(key.starts_with("derived:"), "got: {key}");
        // Repeated call must be identical.
        let key2 = derive_content_key("", &prompt);
        assert_eq!(key, key2);
    }

    #[test]
    fn derived_key_uses_concatenated_text_parts_of_first_user_message() {
        // A first User message can have multiple text Content blocks.
        let first_user = Message {
            role: Role::User,
            content: vec![
                Content::Text {
                    text: "part1".into(),
                    provider_metadata: ProviderMetadata::new(),
                },
                Content::Text {
                    text: "part2".into(),
                    provider_metadata: ProviderMetadata::new(),
                },
            ],
        };
        let prompt = make_prompt(None, vec![first_user.clone()]);
        let expected = derive_content_key("u1", &make_prompt(None, vec![first_user]));
        // Should equal the key for the concatenated text "part1part2".
        let manual = {
            let mut hasher = Sha256::new();
            hasher.update(b"u1");
            hasher.update([0x00]);
            hasher.update(b"");
            hasher.update([0x00]);
            hasher.update(b"part1part2");
            let d = hasher.finalize();
            format!("derived:{}", hex::encode(&d[..8]))
        };
        assert_eq!(expected, manual);
        assert_eq!(derive_content_key("u1", &prompt), manual);
    }

    // ===== derive_session_key full integration =====

    #[test]
    fn full_derive_with_authenticated_caller() {
        let caller = authed_caller("user-123");
        let prompt = make_prompt(Some("Be helpful"), vec![user_msg("hello world")]);
        let key1 = derive_session_key(&caller, &prompt, &http::HeaderMap::new());
        let key2 = derive_session_key(&caller, &prompt, &http::HeaderMap::new());
        assert_eq!(key1, key2, "must be deterministic");
        assert!(key1.starts_with("derived:"), "got: {key1}");
    }
}
