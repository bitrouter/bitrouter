//! OpenAI Codex — `AuthApplier` for the ChatGPT-subscription Codex route.
//!
//! Distinct from the `openai` provider: this targets
//! `chatgpt.com/backend-api/codex` (Responses-only) using an OAuth access
//! token minted by the `bitrouter providers login openai-codex` flow against
//! `auth.openai.com`. The ChatGPT subscription credential does **not**
//! authenticate to `api.openai.com`, so a separate provider id is the
//! cleanest model.
//!
//! Per-request:
//! 1. Read `(openai-codex, target.account_label)` from the credential
//!    store. Must be a `Credential::Oauth` — no API-key path here.
//! 2. Refresh if the access token is within
//!    [`crate::oauth::refresh::REFRESH_WINDOW`] of expiry.
//! 3. Decode the access token JWT to extract `chatgpt_account_id` and
//!    forward it on the `chatgpt-account-id` header alongside the Bearer.
//! 4. Set `OpenAI-Beta: responses=experimental` and `originator: bitrouter`
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

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use crate::oauth::auth_code::AuthCodeError;
use crate::oauth::credential_store::{Credential, CredentialStore, DEFAULT_LABEL, OAuthToken};
use crate::oauth::refresh::{needs_refresh, refresh};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "openai-codex";

/// `AuthApplier` for `provider_name == "openai-codex"`.
pub struct OpenAiCodexAuthApplier {
    store_path: std::path::PathBuf,
    refresh_client: reqwest::Client,
    client_id: String,
    token_endpoint: String,
    cache: Arc<Mutex<std::collections::HashMap<String, OAuthToken>>>,
    /// Per-label single-flight gate around disk-read → refresh →
    /// persist. See [`crate::claude_code::ClaudeCodeAuthApplier`] for the
    /// rationale (concurrent refreshes can have the older refresh_token
    /// invalidated per RFC 6749 §6).
    refresh_gates: Arc<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
}

impl OpenAiCodexAuthApplier {
    /// Build an applier reading the credential store at `store_path` and
    /// using the registry's default Codex OAuth client + token endpoint.
    pub fn new(store_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let registry = crate::oauth::registry::find(PROVIDER_ID).ok_or_else(|| {
            BitrouterError::internal(
                "openai-codex PKCE registry entry is missing — build-time bug".to_string(),
            )
        })?;
        let refresh_client = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                BitrouterError::internal(format!("building Codex OAuth refresh HTTP client: {e}"))
            })?;
        Ok(Self {
            store_path: store_path.into(),
            refresh_client,
            client_id: registry.auth.client_id,
            token_endpoint: registry.auth.token_endpoint,
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            refresh_gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
        })
    }

    /// Tests override the refresh client + endpoint.
    #[cfg(test)]
    pub(crate) fn with_client_and_endpoint(
        store_path: impl Into<std::path::PathBuf>,
        refresh_client: reqwest::Client,
        client_id: impl Into<String>,
        token_endpoint: impl Into<String>,
    ) -> Self {
        Self {
            store_path: store_path.into(),
            refresh_client,
            client_id: client_id.into(),
            token_endpoint: token_endpoint.into(),
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            refresh_gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
        }
    }

    /// Per-label single-flight gate — same shape as the Anthropic applier.
    fn refresh_gate(&self, label: &str) -> Arc<tokio::sync::Mutex<()>> {
        let mut guard = self
            .refresh_gates
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        guard
            .entry(label.to_string())
            .or_insert_with(|| Arc::new(tokio::sync::Mutex::new(())))
            .clone()
    }

    fn cached_fresh(&self, label: &str) -> Option<OAuthToken> {
        let guard = self.cache.lock().ok()?;
        let token = guard.get(label)?;
        (!needs_refresh(token)).then(|| token.clone())
    }

    fn store_in_cache(&self, label: &str, token: &OAuthToken) {
        if let Ok(mut guard) = self.cache.lock() {
            guard.insert(label.to_string(), token.clone());
        }
    }

    async fn resolve_token(&self, label: &str) -> Result<OAuthToken> {
        // 1. Lock-free cache hit.
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(cached);
        }
        // 2. Acquire the per-label gate before any disk or network work.
        let gate = self.refresh_gate(label);
        let _guard = gate.lock().await;
        // 3. Double-checked locking — another task may have refreshed
        //    while we were waiting on the gate.
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(cached);
        }
        let store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reading credential store at {}: {e}",
                self.store_path.display()
            ))
        })?;
        let stored = store
            .get_any(PROVIDER_ID, label)
            .ok_or_else(|| BitrouterError::Upstream {
                status: 401,
                message: format!(
                    "no openai-codex credential for label '{label}' — \
                     run `bitrouter providers login openai-codex`"
                ),
            })?;
        let token = stored
            .as_oauth()
            .cloned()
            .ok_or_else(|| BitrouterError::Upstream {
                status: 401,
                message: format!(
                    "openai-codex credential for '{label}' is an API key — \
                     this provider only accepts subscription OAuth tokens"
                ),
            })?;
        if needs_refresh(&token) {
            let refreshed = refresh(
                &self.refresh_client,
                &self.token_endpoint,
                &self.client_id,
                &token,
            )
            .await
            .map_err(refresh_to_bitrouter_error)?;
            self.persist_refreshed(label, refreshed.clone())?;
            self.store_in_cache(label, &refreshed);
            return Ok(refreshed);
        }
        self.store_in_cache(label, &token);
        Ok(token)
    }

    fn persist_refreshed(&self, label: &str, token: OAuthToken) -> Result<()> {
        let mut store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reloading credential store before refresh write-back: {e}"
            ))
        })?;
        store
            .set(PROVIDER_ID, label, Credential::from_oauth_token(token))
            .map_err(|e| {
                BitrouterError::internal(format!(
                    "persisting refreshed openai-codex OAuth token: {e}"
                ))
            })?;
        Ok(())
    }

    fn label_for<'a>(&self, target: &'a RoutingTarget) -> &'a str {
        target.account_label.as_deref().unwrap_or(DEFAULT_LABEL)
    }
}

fn refresh_to_bitrouter_error(e: AuthCodeError) -> BitrouterError {
    match e {
        AuthCodeError::OAuthError { error, description } => BitrouterError::Upstream {
            status: 401,
            message: format!(
                "openai-codex OAuth refresh failed ({error}{}). Re-run `bitrouter providers login openai-codex`.",
                description.map(|d| format!(": {d}")).unwrap_or_default()
            ),
        },
        other => BitrouterError::Upstream {
            status: 502,
            message: format!("openai-codex OAuth refresh transport error: {other}"),
        },
    }
}

#[async_trait]
impl AuthApplier for OpenAiCodexAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let label = self.label_for(target);
        let token = self.resolve_token(label).await?;
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
            BitrouterError::internal(format!("invalid Codex bearer for Authorization: {e}"))
        })?;
        let headers_mut = request.headers_mut();
        headers_mut.insert(reqwest::header::AUTHORIZATION, auth);
        if let Some(account_id) = account_id {
            let value = HeaderValue::from_str(&account_id).map_err(|e| {
                BitrouterError::internal(format!("invalid chatgpt-account-id header: {e}"))
            })?;
            headers_mut.insert(HeaderName::from_static("chatgpt-account-id"), value);
        }
        headers_mut.insert(
            HeaderName::from_static("openai-beta"),
            HeaderValue::from_static(headers::OPENAI_BETA),
        );
        headers_mut.insert(
            HeaderName::from_static("originator"),
            HeaderValue::from_static(headers::ORIGINATOR),
        );
        headers_mut.insert(
            reqwest::header::USER_AGENT,
            HeaderValue::from_static(headers::USER_AGENT),
        );
        Ok(request)
    }

    async fn prepare_body(
        &self,
        body: &mut serde_json::Value,
        _target: &RoutingTarget,
    ) -> Result<()> {
        // The openai-codex provider always targets the ChatGPT/Codex backend
        // (Responses-only, OAuth-only), so the body always needs the Codex
        // shape — no credential branch required.
        shape_codex_responses_body(body);
        Ok(())
    }
}

/// Shape a Responses request body for the ChatGPT/Codex backend.
///
/// The backend requires `store: false` (it does not persist Codex responses)
/// and `include: ["reasoning.encrypted_content"]` so reasoning models return
/// their encrypted reasoning for multi-turn continuity. The caller's system
/// prompt rides in `instructions` (set by the Responses adapter); when the
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
    obj.insert("store".to_string(), Value::Bool(false));
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
    // prompt — the Codex backend expects it.
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
    use std::path::PathBuf;

    use base64::Engine;
    use base64::engine::general_purpose::URL_SAFE_NO_PAD;
    use bitrouter_sdk::language_model::types::ApiProtocol;
    use wiremock::MockServer;

    use super::*;

    fn tmp_store_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir =
            std::env::temp_dir().join(format!("bitrouter-codex-test-{}-{id}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("creds.json")
    }

    fn codex_target(label: Option<&str>) -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.to_string(),
            service_id: "gpt-5-codex".to_string(),
            api_base: "https://chatgpt.com/backend-api/codex".to_string(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Responses,
            account_label: label.map(String::from),
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    fn make_jwt_with_account(account_id: &str) -> String {
        let header = URL_SAFE_NO_PAD.encode("{}");
        let payload = URL_SAFE_NO_PAD.encode(format!(
            r#"{{"exp":1700000000,"https://api.openai.com/auth":{{"chatgpt_account_id":"{account_id}"}}}}"#
        ));
        let sig = URL_SAFE_NO_PAD.encode("sig");
        format!("{header}.{payload}.{sig}")
    }

    #[tokio::test]
    async fn applies_bearer_account_id_and_integration_headers() {
        let path = tmp_store_path();
        let jwt = make_jwt_with_account("acct-bitrouter");
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: jwt.clone(),
                        expires_at: 0, // non-expiring → no refresh attempt
                        refresh_token: Some("r".into()),
                    }),
                )
                .unwrap();
        }
        let server = MockServer::start().await;
        let applier = OpenAiCodexAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            format!("{}/oauth/token", server.uri()),
        );
        let req = reqwest::Client::new()
            .post("https://chatgpt.com/backend-api/codex/responses")
            .build()
            .unwrap();
        let authed = applier.apply(req, &codex_target(None)).await.unwrap();
        let h = authed.headers();
        assert_eq!(
            h.get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some(format!("Bearer {jwt}").as_str())
        );
        assert_eq!(
            h.get("chatgpt-account-id").and_then(|v| v.to_str().ok()),
            Some("acct-bitrouter")
        );
        assert_eq!(
            h.get("openai-beta").and_then(|v| v.to_str().ok()),
            Some(headers::OPENAI_BETA)
        );
        assert_eq!(
            h.get("originator").and_then(|v| v.to_str().ok()),
            Some(headers::ORIGINATOR)
        );
        assert_eq!(
            h.get(reqwest::header::USER_AGENT)
                .and_then(|v| v.to_str().ok()),
            Some(headers::USER_AGENT)
        );
    }

    #[tokio::test]
    async fn fails_when_no_credential_stored() {
        let path = tmp_store_path();
        let applier = OpenAiCodexAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://chatgpt.com/backend-api/codex/responses")
            .build()
            .unwrap();
        let err = applier.apply(req, &codex_target(None)).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bitrouter providers login openai-codex"),
            "expected helpful hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn rejects_api_key_credential() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(PROVIDER_ID, DEFAULT_LABEL, Credential::api_key("sk-..."))
                .unwrap();
        }
        let applier = OpenAiCodexAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://chatgpt.com/backend-api/codex/responses")
            .build()
            .unwrap();
        let err = applier.apply(req, &codex_target(None)).await.unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("subscription OAuth"),
            "expected API-key rejection, got: {msg}"
        );
    }

    #[tokio::test]
    async fn omits_account_id_header_when_jwt_lacks_claim() {
        let path = tmp_store_path();
        // Plain non-JWT string — claim decode fails gracefully and the
        // applier still sets the Bearer.
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "not-a-jwt".into(),
                        expires_at: 0,
                        refresh_token: None,
                    }),
                )
                .unwrap();
        }
        let applier = OpenAiCodexAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://chatgpt.com/backend-api/codex/responses")
            .build()
            .unwrap();
        let authed = applier.apply(req, &codex_target(None)).await.unwrap();
        assert!(authed.headers().get("chatgpt-account-id").is_none());
        assert!(
            authed
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .is_some()
        );
    }

    #[tokio::test]
    async fn prepare_body_forces_store_false_and_reasoning_include() {
        let path = tmp_store_path();
        let applier = OpenAiCodexAuthApplier::new(&path).unwrap();
        // include absent → created with the reasoning item; store forced false.
        let mut body = serde_json::json!({ "model": "gpt-5-codex", "input": [] });
        applier
            .prepare_body(&mut body, &codex_target(None))
            .await
            .unwrap();
        assert_eq!(body["store"], serde_json::json!(false));
        assert_eq!(
            body["include"],
            serde_json::json!(["reasoning.encrypted_content"])
        );
        // include already present → reasoning item appended without duplication.
        let mut body2 = serde_json::json!({ "include": ["foo"] });
        applier
            .prepare_body(&mut body2, &codex_target(None))
            .await
            .unwrap();
        assert_eq!(
            body2["include"],
            serde_json::json!(["foo", "reasoning.encrypted_content"])
        );
        // Idempotent — a second pass doesn't re-append.
        applier
            .prepare_body(&mut body2, &codex_target(None))
            .await
            .unwrap();
        assert_eq!(
            body2["include"],
            serde_json::json!(["foo", "reasoning.encrypted_content"])
        );
        // No system prompt → instructions defaulted to the Codex fallback.
        assert_eq!(
            body["instructions"],
            serde_json::json!("You are a helpful assistant.")
        );
        // A caller-supplied instructions is preserved untouched.
        let mut body3 = serde_json::json!({ "instructions": "be a pirate", "input": [] });
        applier
            .prepare_body(&mut body3, &codex_target(None))
            .await
            .unwrap();
        assert_eq!(body3["instructions"], serde_json::json!("be a pirate"));

        let mut body4 = serde_json::json!({
            "input": [
                {
                    "type": "message",
                    "role": "system",
                    "content": [{ "type": "input_text", "text": "base rules" }]
                },
                {
                    "type": "message",
                    "role": "developer",
                    "content": "developer rules"
                },
                {
                    "type": "message",
                    "role": "user",
                    "content": [{ "type": "input_text", "text": "hello" }]
                }
            ]
        });
        applier
            .prepare_body(&mut body4, &codex_target(None))
            .await
            .unwrap();
        assert_eq!(
            body4["instructions"],
            serde_json::json!("base rules\n\ndeveloper rules")
        );
        assert_eq!(body4["input"].as_array().unwrap().len(), 1);
        assert_eq!(body4["input"][0]["role"], serde_json::json!("user"));

        let mut body5 = serde_json::json!({
            "input": [],
            "tools": [
                { "type": "custom" },
                { "type": "tool_search" },
                { "type": "function", "name": "read_file", "parameters": {} }
            ]
        });
        applier
            .prepare_body(&mut body5, &codex_target(None))
            .await
            .unwrap();
        assert_eq!(body5["tools"][0]["name"], serde_json::json!("custom"));
        assert_eq!(
            body5["tools"][1]["description"],
            serde_json::json!("Search for available tools.")
        );
        assert_eq!(body5["tools"][1]["parameters"], serde_json::json!({}));
        assert!(body5["tools"][1].get("name").is_none());
        assert_eq!(body5["tools"][2]["name"], serde_json::json!("read_file"));
    }
}
