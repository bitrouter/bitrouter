//! OpenAI Codex — `AuthApplier` for the ChatGPT-subscription Codex route.
//!
//! Distinct from the `openai` provider: this targets
//! `chatgpt.com/backend-api/codex` (Responses-only) using an OAuth access
//! token minted by the `bro providers login openai-codex` flow against
//! `auth.openai.com`. The ChatGPT subscription credential does **not**
//! authenticate to `api.openai.com`, so a separate provider id is the
//! cleanest model.
//!
//! Per-request:
//! 1. Read `(openai-codex, target.account_label)` from the credential
//!    store. Must be a `Credential::Oauth` — no API-key path here.
//! 2. Refresh if the access token is within
//!    [`crate::auth::oauth::REFRESH_WINDOW`] of expiry.
//! 3. Decode the access token JWT to extract `chatgpt_account_id` and
//!    forward it on the `chatgpt-account-id` header alongside the Bearer.
//! 4. Set the current Codex HTTP Responses marker and `originator: bitrouter`
//!    so the upstream admits the request through the Codex pipeline.
//!
//! ## Body shape
//!
//! The ChatGPT/Codex backend requires `store: false` and
//! `include: ["reasoning.encrypted_content"]` on the Responses body.
//! [`OpenAiCodexAuthApplier::prepare_body`] sets both at render time. It also
//! folds any `system` / `developer` message items into `instructions`, because
//! Codex CLI custom-provider traffic can carry those in `input[]` and the
//! ChatGPT Codex backend rejects them there. It also gives `custom` tool
//! declarations a `name` when the generic Responses renderer emitted only
//! `{type:"custom", ...}`; the Codex backend validates `name` for custom tools
//! while rejecting it on some hosted tools. Mirrors OpenClaw
//! `src/llm/providers/openai-chatgpt-responses.ts`.

pub mod headers;
pub mod jwt;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use crate::auth::{AppliedAuth, AuthApplier, CredentialAuthority};
use crate::error::{ModelError, Result};
use crate::target::ModelTarget;

use crate::auth::credentials::OAuthToken;
use crate::auth::store::DEFAULT_ACCOUNT;
use crate::auth::store::{CredentialKey, OAuthSession};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "openai-codex";

/// The subscription backend requires SSE even when the caller wants one result.
/// This selects the upstream mode without changing the caller's source prompt.
pub fn requires_streaming(target: &ModelTarget) -> bool {
    target.provider_name == PROVIDER_ID
}

/// `AuthApplier` for `provider_name == "openai-codex"`.
pub struct OpenAiCodexAuthApplier {
    session: OAuthSession,
}

impl OpenAiCodexAuthApplier {
    /// Bind a selected-account session without file, environment or login discovery.
    pub fn new(session: OAuthSession) -> Self {
        Self { session }
    }

    fn key_for(target: &ModelTarget) -> CredentialKey {
        CredentialKey {
            provider: PROVIDER_ID.into(),
            account: target
                .account_label
                .as_deref()
                .unwrap_or(DEFAULT_ACCOUNT)
                .to_owned(),
        }
    }

    async fn resolve_token(&self, target: &ModelTarget) -> Result<OAuthToken> {
        if target.explicit_credential().is_some() {
            return Ok(OAuthToken {
                access_token: target.api_key.clone(),
                expires_at: 0,
                refresh_token: None,
            });
        }
        self.session.resolve_with_fallback(&Self::key_for(target), target.fallback_credential().map(|access_token| OAuthToken { access_token: access_token.into(), expires_at: 0, refresh_token: None })).await.map_err(|error| match error {
            ModelError::Provider { status: 401, .. } => ModelError::Provider {
                status: 401,
                message: "selected openai-codex account requires subscription OAuth; explicitly authorize this account".into(),
            },
            error => error,
        })
    }

    fn continuation_authority(token: &OAuthToken) -> Option<CredentialAuthority> {
        jwt::decode_codex_claims(&token.access_token)
            .ok()
            .and_then(|claims| claims.chatgpt_account_id)
            .filter(|account_id| !account_id.is_empty())
            .map(|account_id| {
                CredentialAuthority::derive("openai-codex/chatgpt-account", &account_id)
            })
    }
}

fn bearer_access_token(value: Option<&HeaderValue>) -> Option<&str> {
    let value = value?.to_str().ok()?;
    let (scheme, token) = value.split_once(' ')?;
    if scheme.eq_ignore_ascii_case("bearer") {
        let token = token.trim();
        (!token.is_empty()).then_some(token)
    } else {
        None
    }
}

#[async_trait]
impl AuthApplier for OpenAiCodexAuthApplier {
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
        mut request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<AppliedAuth> {
        let token = self.resolve_token(target).await?;
        // The ChatGPT-account-id is namespaced inside the JWT; if the JWT
        // doesn't carry it (test fixtures, an unrelated token) we still
        // attach the Bearer — the upstream will reject and we'll see why.
        // Logging the decode error rather than failing the request keeps
        // a known-incomplete claim from breaking unrelated requests.
        let account_id = jwt::decode_codex_claims(&token.access_token)
            .ok()
            .and_then(|c| c.chatgpt_account_id);
        let bearer = format!("Bearer {}", token.access_token);
        let auth = HeaderValue::from_str(&bearer).map_err(|e| {
            ModelError::configuration(format!("invalid Codex bearer for Authorization: {e}"))
        })?;
        let headers_mut = request.headers_mut();
        headers_mut.insert(reqwest::header::AUTHORIZATION, auth);
        if let Some(account_id) = account_id {
            let value = HeaderValue::from_str(&account_id).map_err(|e| {
                ModelError::configuration(format!("invalid chatgpt-account-id header: {e}"))
            })?;
            headers_mut.insert(HeaderName::from_static("chatgpt-account-id"), value);
        }
        headers_mut.insert(
            HeaderName::from_static("x-openai-internal-codex-responses-lite"),
            HeaderValue::from_static(headers::RESPONSES_LITE),
        );
        headers_mut.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static(headers::ORIGINATOR),
        );
        headers_mut.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(headers::USER_AGENT),
        );
        Ok(match Self::continuation_authority(&token) {
            Some(authority) => AppliedAuth::proven(request, authority),
            None => AppliedAuth::unproven(request),
        })
    }

    async fn continuation_authority(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        let token = self.resolve_token(target).await?;
        Ok(Self::continuation_authority(&token))
    }

    fn output_token_limit_support(&self, _target: &ModelTarget) -> Option<bool> {
        Some(false)
    }

    fn normalize_managed_body(
        &self,
        body: &mut serde_json::Value,
        _target: &ModelTarget,
    ) -> Result<()> {
        shape_codex_responses_body(body);
        Ok(())
    }

    async fn prepare_body(
        &self,
        body: &mut serde_json::Value,
        _target: &ModelTarget,
    ) -> Result<()> {
        // The openai-codex provider always targets the ChatGPT/Codex backend
        // (Responses-only, OAuth-only), so the body always needs the Codex
        // shape — no credential branch required.
        shape_codex_responses_body(body);
        Ok(())
    }

    async fn refresh_after_unauthorized(
        &self,
        target: &ModelTarget,
        rejected_authorization: Option<&HeaderValue>,
    ) -> Result<bool> {
        if target.explicit_credential().is_some() {
            return Ok(false);
        }
        self.session
            .recover(
                &Self::key_for(target),
                bearer_access_token(rejected_authorization),
            )
            .await?;
        Ok(true)
    }
}

/// Shape a Responses request body for the ChatGPT/Codex backend.
///
/// The backend requires `store: false` (it does not persist Codex responses)
/// and `include: ["reasoning.encrypted_content"]` so reasoning models return
/// their encrypted reasoning for multi-turn continuity. The Responses-Lite
/// transport marker also requires `reasoning.context: "all_turns"` and
/// `parallel_tool_calls: false`; preserve any caller effort while pinning that
/// provider contract. Codex clients using a custom gateway emit their local
/// tools at the public top-level `tools` field, while the subscription backend
/// accepts the same definitions as a developer `additional_tools` input item;
/// normalize to that official wire shape. The caller's system prompt rides in
/// `instructions` (set by the Responses adapter); when the
/// caller sent none, default it to the Codex CLI's own fallback so the backend
/// always sees instructions. Mirrors OpenClaw
/// `src/llm/providers/openai-chatgpt-responses.ts`.
fn shape_codex_responses_body(body: &mut serde_json::Value) {
    use serde_json::Value;
    const REASONING_INCLUDE: &str = "reasoning.encrypted_content";
    // OpenClaw: `instructions = systemPrompt || "You are a helpful assistant."`.
    const DEFAULT_INSTRUCTIONS: &str = "You are a helpful assistant.";
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    // The ChatGPT/Codex backend rejects this public Responses parameter even
    // when it came from an OpenAI Chat Completions `max_tokens` request.
    obj.remove("max_output_tokens");
    // Hermes asks Chat Completions streams to include a final usage chunk, but
    // the ChatGPT/Codex backend rejects `stream_options.include_usage`.
    obj.remove("stream_options");
    // Real agent harnesses attach local/session metadata (Codex
    // `client_metadata`, Claude `metadata`) that the ChatGPT/Codex backend does
    // not accept.
    obj.remove("client_metadata");
    obj.remove("metadata");
    obj.remove("thinking");
    obj.remove("context_management");
    obj.remove("output_config");
    obj.remove("cache_control");
    for value in obj.values_mut() {
        strip_codex_unsupported_fields(value);
    }
    obj.insert("store".to_string(), Value::Bool(false));
    obj.insert("parallel_tool_calls".to_string(), Value::Bool(false));
    let reasoning = obj
        .entry("reasoning".to_string())
        .or_insert_with(|| Value::Object(serde_json::Map::new()));
    if !reasoning.is_object() {
        *reasoning = Value::Object(serde_json::Map::new());
    }
    if let Some(reasoning) = reasoning.as_object_mut() {
        reasoning.insert(
            "context".to_string(),
            Value::String("all_turns".to_string()),
        );
    }
    match obj.get_mut("include") {
        Some(Value::Array(items)) => {
            if !items.iter().any(|v| v.as_str() == Some(REASONING_INCLUDE)) {
                items.push(Value::String(REASONING_INCLUDE.to_string()));
            }
        }
        _ => {
            obj.insert(
                "include".to_string(),
                Value::Array(vec![Value::String(REASONING_INCLUDE.to_string())]),
            );
        }
    }
    let lifted_instructions = lift_instruction_messages(obj);
    // Ensure `instructions` is present even when the caller sent no system
    // prompt — the Codex backend expects it. If the caller supplied
    // system/developer messages in `input`, hoist those into instructions
    // because the backend rejects system/developer items inside `input`.
    let has_instructions = obj
        .get("instructions")
        .and_then(Value::as_str)
        .is_some_and(|s| !s.is_empty());
    if !lifted_instructions.is_empty() {
        let lifted = lifted_instructions.join("\n\n");
        let instructions = if has_instructions {
            let existing = obj
                .get("instructions")
                .and_then(Value::as_str)
                .unwrap_or_default();
            format!("{existing}\n\n{lifted}")
        } else {
            lifted
        };
        obj.insert("instructions".to_string(), Value::String(instructions));
    } else if !has_instructions {
        obj.insert(
            "instructions".to_string(),
            Value::String(DEFAULT_INSTRUCTIONS.to_string()),
        );
    }
    ensure_tool_names(obj);
    ensure_tool_descriptions(obj);
    move_tools_to_additional_input(obj);
}

fn strip_codex_unsupported_fields(value: &mut serde_json::Value) {
    use serde_json::Value;
    match value {
        Value::Array(items) => {
            for item in items {
                strip_codex_unsupported_fields(item);
            }
        }
        Value::Object(obj) => {
            obj.remove("cache_control");
            for item in obj.values_mut() {
                strip_codex_unsupported_fields(item);
            }
        }
        _ => {}
    }
}

fn lift_instruction_messages(obj: &mut serde_json::Map<String, serde_json::Value>) -> Vec<String> {
    let Some(serde_json::Value::Array(items)) = obj.get_mut("input") else {
        return Vec::new();
    };
    let mut lifted = Vec::new();
    let mut kept = Vec::with_capacity(items.len());
    for item in std::mem::take(items) {
        if is_instruction_message(&item) {
            if let Some(text) = message_text(&item)
                && !text.trim().is_empty()
            {
                lifted.push(text);
            }
        } else {
            kept.push(item);
        }
    }
    *items = kept;
    lifted
}

fn ensure_tool_names(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(serde_json::Value::Array(tools)) = obj.get_mut("tools") else {
        return;
    };
    for tool in tools {
        let Some(tool_obj) = tool.as_object_mut() else {
            continue;
        };
        let has_name = tool_obj
            .get("name")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if has_name {
            continue;
        }
        if tool_obj.get("type").and_then(serde_json::Value::as_str) == Some("custom") {
            tool_obj.insert(
                "name".to_string(),
                serde_json::Value::String("custom".to_string()),
            );
        }
    }
}

fn ensure_tool_descriptions(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(serde_json::Value::Array(tools)) = obj.get_mut("tools") else {
        return;
    };
    for tool in tools {
        let Some(tool_obj) = tool.as_object_mut() else {
            continue;
        };
        if tool_obj.get("type").and_then(serde_json::Value::as_str) != Some("tool_search") {
            continue;
        }
        let has_description = tool_obj
            .get("description")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|s| !s.is_empty());
        if !has_description {
            tool_obj.insert(
                "description".to_string(),
                serde_json::Value::String("Search for available tools.".to_string()),
            );
        }
        tool_obj
            .entry("parameters".to_string())
            .or_insert_with(|| serde_json::json!({}));
    }
}

fn move_tools_to_additional_input(obj: &mut serde_json::Map<String, serde_json::Value>) {
    let Some(tools) = obj.remove("tools") else {
        return;
    };
    let serde_json::Value::Array(tools) = tools else {
        obj.insert("tools".to_string(), tools);
        return;
    };
    if tools.is_empty() {
        return;
    }
    let Some(serde_json::Value::Array(input)) = obj.get_mut("input") else {
        obj.insert("tools".to_string(), serde_json::Value::Array(tools));
        return;
    };
    if let Some(existing) = input.iter_mut().find(|item| {
        item.get("type").and_then(serde_json::Value::as_str) == Some("additional_tools")
    }) && let Some(existing_tools) = existing
        .get_mut("tools")
        .and_then(serde_json::Value::as_array_mut)
    {
        existing_tools.extend(tools);
        return;
    }
    input.insert(
        0,
        serde_json::json!({
            "type": "additional_tools",
            "role": "developer",
            "tools": tools,
        }),
    );
}

fn is_instruction_message(item: &serde_json::Value) -> bool {
    let is_message = item
        .get("type")
        .and_then(serde_json::Value::as_str)
        .is_none_or(|kind| kind == "message");
    if !is_message {
        return false;
    }
    matches!(
        item.get("role").and_then(serde_json::Value::as_str),
        Some("system" | "developer")
    )
}

fn message_text(item: &serde_json::Value) -> Option<String> {
    let content = item.get("content")?;
    let mut parts = Vec::new();
    collect_text(content, &mut parts);
    (!parts.is_empty()).then(|| parts.join(""))
}

fn collect_text(value: &serde_json::Value, out: &mut Vec<String>) {
    match value {
        serde_json::Value::String(s) => out.push(s.clone()),
        serde_json::Value::Array(items) => {
            for item in items {
                collect_text(item, out);
            }
        }
        serde_json::Value::Object(obj) => {
            if let Some(text) = obj.get("text").and_then(serde_json::Value::as_str) {
                out.push(text.to_string());
            }
        }
        _ => {}
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::{Engine as _, engine::general_purpose::URL_SAFE_NO_PAD};
    fn make_jwt_with_account(account_id: &str) -> String {
        format!(
            "{}.{}.signature",
            URL_SAFE_NO_PAD.encode("{}"),
            URL_SAFE_NO_PAD.encode(format!(
                r#"{{"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#
            ))
        )
    }

    #[test]
    fn routed_codex_input_tools_reach_the_subscription_backend() -> Result<()> {
        use crate::protocol::responses::ResponsesAdapter;
        use crate::protocol::{InboundAdapter, OutboundAdapter};

        let declarations = serde_json::json!([{
            "type": "namespace", "name": "functions", "tools": [{
                "type": "custom", "name": "exec", "description": "Read workspace files",
                "format": {"type": "text"}
            }]
        }]);
        let adapter = ResponsesAdapter;
        let prompt = adapter.parse_request(serde_json::json!({
            "model": "m",
            "input": [
                {"type": "additional_tools", "role": "developer", "tools": declarations},
                {"role": "user", "content": "Read README.md"}
            ]
        }))?;
        let mut body = adapter.render_request(&prompt)?;
        shape_codex_responses_body(&mut body);
        assert_eq!(body["input"][0]["type"], "additional_tools");
        assert_eq!(body["input"][0]["tools"], declarations);
        assert_eq!(body["input"][1]["content"][0]["text"], "Read README.md");
        Ok(())
    }

    #[test]
    fn continuation_authority_tracks_account_not_rotating_token()
    -> std::result::Result<(), Box<dyn std::error::Error>> {
        let first_access = make_jwt_with_account("acct-stable");
        let rotated_access = format!(
            "{}.rotated-signature",
            first_access.rsplit_once('.').ok_or("missing signature")?.0
        );
        let token = |access_token: String| OAuthToken {
            access_token,
            expires_at: 0,
            refresh_token: Some("rotating-refresh-token".into()),
        };

        assert_eq!(
            OpenAiCodexAuthApplier::continuation_authority(&token(first_access)),
            OpenAiCodexAuthApplier::continuation_authority(&token(rotated_access))
        );
        assert_ne!(
            OpenAiCodexAuthApplier::continuation_authority(&token(make_jwt_with_account(
                "acct-stable"
            ))),
            OpenAiCodexAuthApplier::continuation_authority(&token(make_jwt_with_account(
                "acct-replaced"
            )))
        );
        Ok(())
    }
}
