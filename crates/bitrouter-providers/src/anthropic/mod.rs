//! Anthropic — dual-mode `AuthApplier` covering both API-key auth and the
//! Claude Pro/Max OAuth subscription path.
//!
//! Same provider id (`"anthropic"`) for both modes: at request time the
//! applier looks up `(anthropic, target.account_label)` in the
//! [`crate::oauth::credential_store::CredentialStore`] and branches on the
//! credential type.
//!
//! | Stored credential | Outbound headers |
//! |---|---|
//! | `Credential::Oauth` (Claude Pro/Max)  | `Authorization: Bearer sk-ant-oat…`, `anthropic-beta: claude-code-20250219,oauth-2025-04-20`, `anthropic-version: 2023-06-01`, `user-agent: claude-cli/…`, `x-app: cli`. **No `x-api-key`** — the upstream rejects OAuth requests that also carry `x-api-key`. The request body is shaped by [`AnthropicOAuthApplier::prepare_body`] so Claude Code's identity is the first `system` block. |
//! | `Credential::ApiKey`                  | `x-api-key: <value>`, `anthropic-version: 2023-06-01`. |
//! | _no credential in store_              | Fall back to the routing target's `api_key` field (the env-var path). Existing `${ANTHROPIC_API_KEY}` setups behave exactly as before. |
//!
//! The OAuth branch refreshes the access token via
//! [`crate::oauth::refresh::refresh`] if it's within
//! [`crate::oauth::refresh::REFRESH_WINDOW`] of expiry, writes the new token
//! back to the store, and caches it in memory to avoid hammering the
//! refresh endpoint under load.
//!
//! ## Body shape
//!
//! The Claude Pro/Max subscription endpoint admits requests under the Claude
//! Code agent profile: the first `system` block has to be Claude Code's
//! identity string. [`AnthropicOAuthApplier::prepare_body`] enforces that at
//! render time (when the body is still a `serde_json::Value`), prepending the
//! identity block and preserving the caller's own system prompt after it.
//! API-key requests pass the body through unchanged.
//!
//! Authoritative reference for the OAuth client + headers: Claude Code's
//! published OAuth client (client_id `9d1c250a-…`), as reused by
//! [`@earendil-works/pi-ai`](https://github.com/openclaw/openclaw)'s
//! Anthropic login and OpenCode's auth registry.

pub mod headers;

use std::sync::Arc;
use std::sync::Mutex;
use std::time::Duration;

use async_trait::async_trait;
use reqwest::header::{HeaderName, HeaderValue};

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use crate::import::claude_code::ClaudeCodeStore;
use crate::oauth::auth_code::AuthCodeError;
use crate::oauth::credential_store::{Credential, CredentialStore, DEFAULT_LABEL, OAuthToken};
use crate::oauth::refresh::{needs_refresh, refresh};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "anthropic";

/// `AuthApplier` for `provider_name == "anthropic"`.
///
/// The applier owns:
/// - the credential-store path so it can read + refresh-write back; and
/// - the OAuth client id + token endpoint pulled from
///   [`crate::oauth::registry::find`] at construction.
///
/// In-memory the applier caches the freshly-refreshed `OAuthToken` keyed
/// by `account_label` so concurrent requests don't all hit the refresh
/// endpoint.
pub struct AnthropicOAuthApplier {
    store_path: std::path::PathBuf,
    refresh_client: reqwest::Client,
    /// OAuth client id used for the refresh grant. Mirrors what the
    /// `bitrouter login anthropic` flow wrote.
    client_id: String,
    /// Token endpoint used for the refresh grant.
    token_endpoint: String,
    /// `account_label -> freshest OAuthToken` cache. Populated on first
    /// refresh; subsequent requests within the validity window reuse it
    /// without touching disk or the refresh endpoint.
    cache: Arc<Mutex<std::collections::HashMap<String, OAuthToken>>>,
    /// Per-label single-flight gate around the disk-read → refresh →
    /// persist sequence. RFC 6749 §6 lets the server invalidate older
    /// refresh tokens once a new one is minted, so two concurrent refreshes
    /// of the same label can silently log the user out. Holding this
    /// `tokio::sync::Mutex` across the refresh `await` serialises them;
    /// the second waiter re-checks the cache after acquiring the lock
    /// (double-checked locking) and skips the refresh if the first one
    /// already populated it.
    refresh_gates: Arc<Mutex<std::collections::HashMap<String, Arc<tokio::sync::Mutex<()>>>>>,
    /// Live view of Claude Code's own credential store (`~/.claude`). Used to
    /// resolve a [`Credential::ClaudeCodeCli`] marker: read the token live and
    /// write any refresh back to the same source, so bitrouter and Claude Code
    /// share one credential. `None` only when no home directory resolves.
    claude_code: Option<ClaudeCodeStore>,
}

impl AnthropicOAuthApplier {
    /// Build an applier that reads + writes the credential store at
    /// `store_path` and refreshes tokens using the registry's default
    /// Anthropic OAuth client + token endpoint.
    pub fn new(store_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        let registry = crate::oauth::registry::find(PROVIDER_ID).ok_or_else(|| {
            BitrouterError::internal(
                "anthropic PKCE registry entry is missing — this is a build-time bug".to_string(),
            )
        })?;
        let refresh_client = reqwest::Client::builder()
            .user_agent(concat!("bitrouter-providers/", env!("CARGO_PKG_VERSION")))
            .timeout(Duration::from_secs(30))
            .build()
            .map_err(|e| {
                BitrouterError::internal(format!(
                    "building Anthropic OAuth refresh HTTP client: {e}"
                ))
            })?;
        Ok(Self {
            store_path: store_path.into(),
            refresh_client,
            client_id: registry.auth.client_id,
            token_endpoint: registry.auth.token_endpoint,
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            refresh_gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
            claude_code: ClaudeCodeStore::system(),
        })
    }

    /// Override the refresh client + token endpoint (tests use this against
    /// `wiremock`).
    #[cfg(test)]
    pub(crate) fn with_client_and_endpoint(
        store_path: impl Into<std::path::PathBuf>,
        refresh_client: reqwest::Client,
        client_id: impl Into<String>,
        token_endpoint: impl Into<String>,
        claude_code: Option<ClaudeCodeStore>,
    ) -> Self {
        Self {
            store_path: store_path.into(),
            refresh_client,
            client_id: client_id.into(),
            token_endpoint: token_endpoint.into(),
            cache: Arc::new(Mutex::new(std::collections::HashMap::new())),
            refresh_gates: Arc::new(Mutex::new(std::collections::HashMap::new())),
            claude_code,
        }
    }

    /// Per-label single-flight gate. Cloned out under the std mutex (no
    /// awaits held); the returned `tokio::sync::Mutex` serialises the
    /// disk-read → refresh → persist sequence for that label.
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

    /// Load the stored credential for the given account label, refreshing
    /// the inner OAuth access token if it's within the refresh window.
    /// Returns `None` when no credential is stored under that label —
    /// callers fall through to the routing-target's inline `api_key`.
    ///
    /// Concurrency: cache hits are lock-free. The disk-read → refresh →
    /// persist sequence is single-flighted per label via
    /// [`Self::refresh_gate`] so concurrent requests don't both POST to
    /// the token endpoint (and risk having the server invalidate the
    /// older refresh token, per RFC 6749 §6).
    async fn resolve_credential(&self, label: &str) -> Result<Option<ResolvedCredential>> {
        // 1. Cheap in-memory cache check — no locks held across awaits.
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(Some(ResolvedCredential::Oauth(cached)));
        }
        // 2. Acquire the per-label single-flight gate before any disk
        //    read or refresh POST. If another task is mid-refresh for
        //    this label, we wait here and then short-circuit on the
        //    cache.
        let gate = self.refresh_gate(label);
        let _guard = gate.lock().await;
        // 3. Double-checked locking — another task may have just
        //    refreshed and populated the cache while we were waiting on
        //    the gate.
        if let Some(cached) = self.cached_fresh(label) {
            return Ok(Some(ResolvedCredential::Oauth(cached)));
        }
        // 4. Disk read.
        let store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reading credential store at {}: {e}",
                self.store_path.display()
            ))
        })?;
        let cred = match store.get_any(PROVIDER_ID, label) {
            Some(c) => c.clone(),
            None => return Ok(None),
        };
        // 5. ApiKey: no refresh logic, return as-is. Marker: resolve live from
        //    the Claude Code store (read + refresh-write-back there).
        let token = match cred {
            Credential::ApiKey { value } => {
                return Ok(Some(ResolvedCredential::ApiKey(value)));
            }
            Credential::ClaudeCodeCli => {
                return self.resolve_claude_code_session(label).await;
            }
            Credential::Oauth(t) => t,
        };
        // 6. OAuth: refresh if expiring.
        if needs_refresh(&token) {
            let refreshed = refresh(
                &self.refresh_client,
                &self.token_endpoint,
                &self.client_id,
                &token,
            )
            .await
            .map_err(refresh_to_bitrouter_error)?;
            // Persist back to disk so other processes / reloads see it.
            self.persist_refreshed(label, refreshed.clone())?;
            self.store_in_cache(label, &refreshed);
            return Ok(Some(ResolvedCredential::Oauth(refreshed)));
        }
        self.store_in_cache(label, &token);
        Ok(Some(ResolvedCredential::Oauth(token)))
    }

    /// Resolve a [`Credential::ClaudeCodeCli`] marker: read the live token from
    /// Claude Code's own store (`~/.claude`), refreshing it in place when it is
    /// within the refresh window and **writing the rotated token back to that
    /// same store** so bitrouter and Claude Code never diverge (RFC 6749 §6
    /// refresh-token rotation would otherwise family-revoke one of them).
    ///
    /// Called under the per-label single-flight gate, so the read → refresh →
    /// write-back is serialised; the live read also picks up any refresh the
    /// `claude` CLI performed in the meantime, avoiding a redundant rotation.
    async fn resolve_claude_code_session(&self, label: &str) -> Result<Option<ResolvedCredential>> {
        let store = self
            .claude_code
            .as_ref()
            .ok_or_else(|| BitrouterError::Upstream {
                status: 401,
                message: "cannot locate the Claude Code session (no home directory) — set HOME \
                      or run `bitrouter login anthropic`"
                    .into(),
            })?;
        let live = store
            .read()
            .map_err(|e| BitrouterError::internal(format!("reading Claude Code session: {e}")))?;
        let Some(live) = live else {
            return Err(BitrouterError::Upstream {
                status: 401,
                message: "no Claude Code session found — run `claude auth login` (or \
                          `bitrouter login anthropic`) to sign in to your Claude subscription"
                    .into(),
            });
        };
        let token = if needs_refresh(&live.token) {
            let refreshed = refresh(
                &self.refresh_client,
                &self.token_endpoint,
                &self.client_id,
                &live.token,
            )
            .await
            .map_err(refresh_to_bitrouter_error)?;
            // Single source of truth: write the rotation back where we read it.
            store.write_back(&refreshed, &live.source).map_err(|e| {
                BitrouterError::internal(format!(
                    "writing refreshed token back to the Claude Code store: {e}"
                ))
            })?;
            refreshed
        } else {
            live.token
        };
        // Cache the resolved token to collapse the in-request double resolution
        // (`apply` + `prepare_body`) and avoid a `security`/file read per
        // request. The cache self-invalidates at the refresh window, so a
        // `claude`-side rotation is picked up by the next refresh. A
        // non-expiring token (`expires_at == 0`) is deliberately NOT cached: it
        // would otherwise be served for the whole process lifetime without ever
        // re-reading the live store, defeating the marker's "single source of
        // truth" intent — so we re-read it live on each request instead.
        if token.expires_at > 0 {
            self.store_in_cache(label, &token);
        }
        Ok(Some(ResolvedCredential::Oauth(token)))
    }

    fn persist_refreshed(&self, label: &str, token: OAuthToken) -> Result<()> {
        let mut store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reloading credential store before refresh write-back at {}: {e}",
                self.store_path.display()
            ))
        })?;
        store
            .set(PROVIDER_ID, label, Credential::from_oauth_token(token))
            .map_err(|e| {
                BitrouterError::internal(format!("persisting refreshed anthropic OAuth token: {e}"))
            })?;
        Ok(())
    }

    fn label_for<'a>(&self, target: &'a RoutingTarget) -> &'a str {
        target.account_label.as_deref().unwrap_or(DEFAULT_LABEL)
    }
}

enum ResolvedCredential {
    Oauth(OAuthToken),
    ApiKey(String),
}

fn refresh_to_bitrouter_error(e: AuthCodeError) -> BitrouterError {
    match e {
        AuthCodeError::OAuthError { error, description } => BitrouterError::Upstream {
            status: 401,
            message: format!(
                "anthropic OAuth refresh failed ({error}{}). Re-run `bitrouter login anthropic`.",
                description.map(|d| format!(": {d}")).unwrap_or_default()
            ),
        },
        other => BitrouterError::Upstream {
            status: 502,
            message: format!("anthropic OAuth refresh transport error: {other}"),
        },
    }
}

#[async_trait]
impl AuthApplier for AnthropicOAuthApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let label = self.label_for(target);
        let resolved = self.resolve_credential(label).await?;
        // `anthropic-version` is mandatory regardless of credential type.
        request.headers_mut().insert(
            "anthropic-version",
            HeaderValue::from_static(headers::ANTHROPIC_VERSION),
        );
        match resolved {
            Some(ResolvedCredential::Oauth(token)) => {
                let bearer = format!("Bearer {}", token.access_token);
                let auth = HeaderValue::from_str(&bearer).map_err(|e| {
                    BitrouterError::internal(format!(
                        "invalid anthropic OAuth bearer for Authorization: {e}"
                    ))
                })?;
                let headers_mut = request.headers_mut();
                headers_mut.insert(reqwest::header::AUTHORIZATION, auth);
                // OAuth requests must NOT carry x-api-key — the upstream
                // returns 401 when both auth schemes are present.
                headers_mut.remove("x-api-key");
                // Merge — never overwrite — the OAuth-required betas with any
                // the client already sent. Claude Code appends feature betas
                // (e.g. `context-management-2025-06-27`, interleaved thinking,
                // prompt caching) alongside the matching request-body fields;
                // clobbering the header would leave those fields with no
                // enabling beta and the upstream 400s ("Extra inputs are not
                // permitted").
                let client_betas: Vec<String> = headers_mut
                    .get_all("anthropic-beta")
                    .iter()
                    .filter_map(|v| v.to_str().ok())
                    .map(str::to_string)
                    .collect();
                let beta_value = merged_beta_value(client_betas.iter().map(String::as_str));
                let beta_header = HeaderValue::from_str(&beta_value).map_err(|e| {
                    BitrouterError::internal(format!("invalid anthropic-beta header: {e}"))
                })?;
                headers_mut.insert(HeaderName::from_static("anthropic-beta"), beta_header);
                // The subscription endpoint expects first-party-CLI-shaped
                // requests; mirror Claude Code's user-agent + x-app so the
                // OAuth credential is admitted. (Reference: OpenClaw
                // `src/llm/providers/anthropic.ts`.)
                headers_mut.insert(
                    reqwest::header::USER_AGENT,
                    HeaderValue::from_static(headers::CLAUDE_CODE_USER_AGENT),
                );
                headers_mut.insert(
                    HeaderName::from_static("x-app"),
                    HeaderValue::from_static(headers::CLAUDE_CODE_X_APP),
                );
                Ok(request)
            }
            Some(ResolvedCredential::ApiKey(value)) => {
                apply_api_key_header(&mut request, &value)?;
                Ok(request)
            }
            None => {
                // No store entry — fall through to the routing-target's
                // configured key (env-var path). Mirrors what
                // `MessagesTransport::authorise` would have done.
                let key = target.effective_api_key();
                if key.is_empty() {
                    return Err(BitrouterError::Upstream {
                        status: 401,
                        message: "no anthropic credential — set ANTHROPIC_API_KEY or run \
                             `bitrouter login anthropic`"
                            .into(),
                    });
                }
                apply_api_key_header(&mut request, key)?;
                Ok(request)
            }
        }
    }

    async fn prepare_body(
        &self,
        body: &mut serde_json::Value,
        target: &RoutingTarget,
    ) -> Result<()> {
        // Only the OAuth (Claude Pro/Max) path needs the Claude Code identity
        // block; API-key and env-var requests pass the caller's body through
        // unchanged. For an OAuth credential the resolution is cached and
        // shared with `apply`; the non-OAuth paths re-read the store (cheap)
        // instead of caching, so a credential added between requests is picked
        // up without a restart.
        let label = self.label_for(target);
        if !matches!(
            self.resolve_credential(label).await?,
            Some(ResolvedCredential::Oauth(_))
        ) {
            return Ok(());
        }
        inject_claude_code_identity(body);
        Ok(())
    }
}

/// Merge the OAuth-required `anthropic-beta` values (which the Claude Pro/Max
/// subscription endpoint demands) with any the client already sent, deduping
/// while keeping the required values first. Real Claude Code traffic carries
/// feature betas next to matching request-body fields, so the union — not an
/// overwrite — is what keeps those requests valid upstream.
fn merged_beta_value<'a>(client_betas: impl Iterator<Item = &'a str>) -> String {
    let mut out: Vec<String> = headers::OAUTH_BETA_VALUES
        .iter()
        .map(|s| (*s).to_string())
        .collect();
    for raw in client_betas {
        for beta in raw.split(',') {
            let beta = beta.trim();
            if !beta.is_empty() && !out.iter().any(|x| x == beta) {
                out.push(beta.to_string());
            }
        }
    }
    out.join(",")
}

fn apply_api_key_header(request: &mut reqwest::Request, key: &str) -> Result<()> {
    let value = HeaderValue::from_str(key).map_err(|e| {
        BitrouterError::internal(format!("invalid api key for x-api-key header: {e}"))
    })?;
    request.headers_mut().insert("x-api-key", value);
    // OAuth and API-key paths must not both be set; clear any stale
    // Bearer the protocol layer might have added.
    request.headers_mut().remove(reqwest::header::AUTHORIZATION);
    Ok(())
}

/// Ensure the first Anthropic `system` block is Claude Code's identity string,
/// preserving the caller's own system prompt after it. Idempotent — a body
/// whose first block is already the identity is left unchanged.
///
/// Required by the Claude Pro/Max subscription endpoint, which admits requests
/// under the Claude Code agent profile (reference: OpenClaw
/// `src/llm/providers/anthropic.ts`). The caller's system prompt is never
/// dropped; it follows the identity block.
fn inject_claude_code_identity(body: &mut serde_json::Value) {
    use serde_json::Value;
    let Some(obj) = body.as_object_mut() else {
        return;
    };
    let identity_block = serde_json::json!({
        "type": "text",
        "text": headers::CLAUDE_CODE_SYSTEM_PROMPT,
    });
    let new_system = match obj.remove("system") {
        // No system yet → the identity as a single content block, matching
        // Claude Code's own wire shape (it always sends `system` as an array).
        None => Value::Array(vec![identity_block]),
        // Already correct → leave it.
        Some(Value::String(s)) if s == headers::CLAUDE_CODE_SYSTEM_PROMPT => Value::String(s),
        // A plain-string system prompt → [identity, caller's prompt].
        Some(Value::String(s)) => Value::Array(vec![
            identity_block,
            serde_json::json!({ "type": "text", "text": s }),
        ]),
        // A content-block array → prepend the identity unless already present.
        Some(Value::Array(mut blocks)) => {
            if !first_block_is_identity(&blocks) {
                blocks.insert(0, identity_block);
            }
            Value::Array(blocks)
        }
        // Any other shape (not valid for Messages) → wrap defensively so the
        // identity still leads.
        Some(other) => Value::Array(vec![identity_block, other]),
    };
    obj.insert("system".to_string(), new_system);
}

/// Whether the first element of an Anthropic `system` content-block array is
/// already Claude Code's identity string.
fn first_block_is_identity(blocks: &[serde_json::Value]) -> bool {
    blocks
        .first()
        .and_then(|b| b.get("text"))
        .and_then(|t| t.as_str())
        == Some(headers::CLAUDE_CODE_SYSTEM_PROMPT)
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bitrouter_sdk::language_model::types::ApiProtocol;
    use wiremock::MockServer;

    use super::*;
    use crate::oauth::credential_store::OAuthToken;

    fn tmp_store_path() -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-anthropic-test-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir.join("creds.json")
    }

    fn anthropic_target(label: Option<&str>) -> RoutingTarget {
        RoutingTarget {
            provider_name: PROVIDER_ID.to_string(),
            service_id: "claude-opus-4-7".to_string(),
            api_base: "https://api.anthropic.com/v1".to_string(),
            api_key: String::new(),
            api_protocol: ApiProtocol::Messages,
            account_label: label.map(String::from),
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        }
    }

    fn anthropic_target_with_env_key(key: &str) -> RoutingTarget {
        let mut t = anthropic_target(None);
        t.api_key = key.to_string();
        t
    }

    #[tokio::test]
    async fn fallthrough_uses_target_api_key_when_store_is_empty() {
        let path = tmp_store_path();
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let target = anthropic_target_with_env_key("sk-ant-api03-env");
        let authed = applier.apply(req, &target).await.unwrap();
        let h = authed.headers();
        assert_eq!(
            h.get("x-api-key").and_then(|v| v.to_str().ok()),
            Some("sk-ant-api03-env")
        );
        assert_eq!(
            h.get("anthropic-version").and_then(|v| v.to_str().ok()),
            Some(headers::ANTHROPIC_VERSION)
        );
        assert!(h.get(reqwest::header::AUTHORIZATION).is_none());
    }

    #[tokio::test]
    async fn errors_when_no_credential_anywhere() {
        let path = tmp_store_path();
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let err = applier
            .apply(req, &anthropic_target(None))
            .await
            .unwrap_err();
        let msg = err.to_string();
        assert!(
            msg.contains("bitrouter login anthropic"),
            "expected helpful hint, got: {msg}"
        );
    }

    #[tokio::test]
    async fn stored_api_key_overrides_target_fallthrough() {
        let path = tmp_store_path();
        // Seed an API key in the store.
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::api_key("sk-ant-api03-from-store"),
                )
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let authed = applier
            .apply(req, &anthropic_target_with_env_key("env-key-shadowed"))
            .await
            .unwrap();
        assert_eq!(
            authed
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok()),
            Some("sk-ant-api03-from-store")
        );
    }

    #[tokio::test]
    async fn stored_oauth_token_applies_bearer_and_strips_x_api_key() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "sk-ant-oat-fresh".into(),
                        expires_at: 0, // non-expiring → no refresh attempt
                        refresh_token: Some("r".into()),
                    }),
                )
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let mut req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        // Pretend the protocol adapter already set x-api-key; the OAuth
        // path must strip it.
        req.headers_mut()
            .insert("x-api-key", HeaderValue::from_static("stale-key"));
        let authed = applier.apply(req, &anthropic_target(None)).await.unwrap();
        let h = authed.headers();
        assert_eq!(
            h.get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer sk-ant-oat-fresh")
        );
        assert!(h.get("x-api-key").is_none());
        let beta = h
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(beta.contains("oauth-2025-04-20"));
        assert!(beta.contains("claude-code-20250219"));
        assert_eq!(
            h.get(reqwest::header::USER_AGENT)
                .and_then(|v| v.to_str().ok()),
            Some(headers::CLAUDE_CODE_USER_AGENT)
        );
        assert_eq!(
            h.get("x-app").and_then(|v| v.to_str().ok()),
            Some(headers::CLAUDE_CODE_X_APP)
        );
    }

    #[tokio::test]
    async fn oauth_merges_client_anthropic_beta_instead_of_overwriting() {
        // Real Claude Code traffic appends feature betas
        // (`context-management-…`, interleaved-thinking) next to the matching
        // request-body fields. The applier must keep them while adding the
        // OAuth-required betas — overwriting strips them and the upstream 400s
        // ("Extra inputs are not permitted") on the now-orphaned body field.
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "sk-ant-oat".into(),
                        expires_at: 0,
                        refresh_token: Some("r".into()),
                    }),
                )
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let mut req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        req.headers_mut().insert(
            "anthropic-beta",
            HeaderValue::from_static(
                "context-management-2025-06-27,interleaved-thinking-2025-05-14",
            ),
        );
        let authed = applier.apply(req, &anthropic_target(None)).await.unwrap();
        let beta = authed
            .headers()
            .get("anthropic-beta")
            .and_then(|v| v.to_str().ok())
            .unwrap();
        assert!(
            beta.contains("oauth-2025-04-20"),
            "required beta dropped: {beta}"
        );
        assert!(
            beta.contains("claude-code-20250219"),
            "required beta dropped: {beta}"
        );
        assert!(
            beta.contains("context-management-2025-06-27"),
            "client beta dropped: {beta}"
        );
        assert!(
            beta.contains("interleaved-thinking-2025-05-14"),
            "client beta dropped: {beta}"
        );
    }

    #[tokio::test]
    async fn fresh_oauth_token_skips_refresh() {
        // Sanity check on the cache-hit path: a long-lived token (1h
        // ahead of the 60s refresh window) is reused directly. The end-
        // to-end refresh round-trip is covered in
        // `oauth::refresh::tests::refresh_returns_new_access_token`.
        let path = tmp_store_path();
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "still-fresh".into(),
                        expires_at: now + 3600,
                        refresh_token: Some("ignored".into()),
                    }),
                )
                .unwrap();
        }
        // Point the refresh endpoint at a wiremock that fails the test
        // if it's hit at all (no mounted responder → wiremock 404s).
        let server = MockServer::start().await;
        let applier = AnthropicOAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            format!("{}/oauth/token", server.uri()),
            None,
        );
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let authed = applier.apply(req, &anthropic_target(None)).await.unwrap();
        assert_eq!(
            authed
                .headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer still-fresh")
        );
    }

    #[tokio::test]
    async fn multi_account_lookup_uses_target_label() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(PROVIDER_ID, "pro-max", Credential::api_key("for-pro-max"))
                .unwrap();
            store
                .set(PROVIDER_ID, "work-key", Credential::api_key("for-work"))
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let authed = applier
            .apply(req, &anthropic_target(Some("pro-max")))
            .await
            .unwrap();
        assert_eq!(
            authed
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok()),
            Some("for-pro-max")
        );
        let req2 = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let authed2 = applier
            .apply(req2, &anthropic_target(Some("work-key")))
            .await
            .unwrap();
        assert_eq!(
            authed2
                .headers()
                .get("x-api-key")
                .and_then(|v| v.to_str().ok()),
            Some("for-work")
        );
    }

    #[tokio::test]
    async fn oauth_prepare_body_injects_claude_code_identity() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::from_oauth_token(OAuthToken {
                        access_token: "sk-ant-oat-x".into(),
                        expires_at: 0,
                        refresh_token: Some("r".into()),
                    }),
                )
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        // No system field → identity as a single-element content-block array.
        let mut body = serde_json::json!({ "model": "claude", "messages": [] });
        applier
            .prepare_body(&mut body, &anthropic_target(None))
            .await
            .unwrap();
        let only = body["system"].as_array().unwrap();
        assert_eq!(only.len(), 1);
        assert_eq!(only[0]["text"], headers::CLAUDE_CODE_SYSTEM_PROMPT);
        // String system → array: identity first, caller's prompt preserved.
        let mut body2 = serde_json::json!({ "system": "be terse", "messages": [] });
        applier
            .prepare_body(&mut body2, &anthropic_target(None))
            .await
            .unwrap();
        let arr = body2["system"].as_array().unwrap();
        assert_eq!(arr[0]["text"], headers::CLAUDE_CODE_SYSTEM_PROMPT);
        assert_eq!(arr[1]["text"], "be terse");
        // Idempotent — a second pass doesn't double-prepend.
        applier
            .prepare_body(&mut body2, &anthropic_target(None))
            .await
            .unwrap();
        assert_eq!(body2["system"].as_array().unwrap().len(), 2);
    }

    /// Write a `.credentials.json` in a fresh temp dir and return its path.
    fn tmp_claude_creds(contents: &str) -> PathBuf {
        use std::sync::atomic::{AtomicU64, Ordering};
        static COUNTER: AtomicU64 = AtomicU64::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "bitrouter-anthropic-cc-{}-{id}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join(".credentials.json");
        std::fs::write(&path, contents).unwrap();
        path
    }

    fn seed_marker(store_path: &std::path::Path) {
        let mut store = CredentialStore::load(store_path).unwrap();
        store
            .set(PROVIDER_ID, DEFAULT_LABEL, Credential::ClaudeCodeCli)
            .unwrap();
    }

    #[tokio::test]
    async fn claude_code_cli_marker_applies_bearer_from_live_store() {
        // Marker in bitrouter's store + a non-expiring live Claude Code session
        // (no `expiresAt` → never refreshed) → the live access token is applied
        // as a Bearer and any stale x-api-key is stripped.
        let path = tmp_store_path();
        seed_marker(&path);
        let creds = tmp_claude_creds(
            r#"{"claudeAiOauth":{"accessToken":"sk-ant-oat-live","refreshToken":"r"}}"#,
        );
        let applier = AnthropicOAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            "https://example.com/oauth/token",
            Some(ClaudeCodeStore::file_only(&creds)),
        );
        let mut req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        req.headers_mut()
            .insert("x-api-key", HeaderValue::from_static("stale"));
        let authed = applier.apply(req, &anthropic_target(None)).await.unwrap();
        let h = authed.headers();
        assert_eq!(
            h.get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok()),
            Some("Bearer sk-ant-oat-live")
        );
        assert!(h.get("x-api-key").is_none());
        assert!(
            h.get("anthropic-beta")
                .and_then(|v| v.to_str().ok())
                .unwrap()
                .contains("oauth-2025-04-20")
        );
    }

    #[tokio::test]
    async fn claude_code_cli_marker_missing_session_errors() {
        // Marker present but no live Claude Code session → a helpful 401 that
        // points the user at `claude auth login`, not a silent fall-through.
        let path = tmp_store_path();
        seed_marker(&path);
        let absent = std::env::temp_dir().join("bitrouter-anthropic-cc-absent/none.json");
        let applier = AnthropicOAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            "https://example.com/oauth/token",
            Some(ClaudeCodeStore::file_only(&absent)),
        );
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let err = applier
            .apply(req, &anthropic_target(None))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("claude auth login"),
            "expected a login hint, got: {err}"
        );
    }

    #[tokio::test]
    async fn claude_code_cli_marker_expiring_token_triggers_refresh() {
        // An expiring live token must drive a refresh attempt rather than serve
        // the stale token. Pointing the endpoint at an insecure (http) URL makes
        // `refresh` fail fast with a typed error, proving the needs_refresh
        // branch is taken. (The happy-path refresh and write-back are covered by
        // `oauth::refresh::tests` and `import::claude_code::tests` respectively;
        // an http MockServer can't exercise them because `refresh` requires
        // https.)
        let path = tmp_store_path();
        seed_marker(&path);
        let creds = tmp_claude_creds(
            r#"{"claudeAiOauth":{"accessToken":"old","refreshToken":"RT","expiresAt":1000}}"#,
        );
        let applier = AnthropicOAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            "http://insecure.example.com/oauth/token",
            Some(ClaudeCodeStore::file_only(&creds)),
        );
        let req = reqwest::Client::new()
            .post("https://api.anthropic.com/v1/messages")
            .build()
            .unwrap();
        let err = applier
            .apply(req, &anthropic_target(None))
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("refresh"),
            "expected a refresh error proving the refresh path ran, got: {err}"
        );
    }

    #[tokio::test]
    async fn claude_code_cli_marker_non_expiring_is_reread_live_each_request() {
        // A non-expiring live token must NOT be cached for the process lifetime:
        // when `claude` rotates the on-disk token, the next request must see the
        // new value, keeping bitrouter in lockstep with the single source of
        // truth.
        let path = tmp_store_path();
        seed_marker(&path);
        let creds =
            tmp_claude_creds(r#"{"claudeAiOauth":{"accessToken":"first","refreshToken":"r"}}"#);
        let applier = AnthropicOAuthApplier::with_client_and_endpoint(
            &path,
            reqwest::Client::new(),
            "client-1",
            "https://example.com/oauth/token",
            Some(ClaudeCodeStore::file_only(&creds)),
        );
        let bearer = |req: reqwest::Request| {
            req.headers()
                .get(reqwest::header::AUTHORIZATION)
                .and_then(|v| v.to_str().ok())
                .map(str::to_string)
        };
        let first = applier
            .apply(
                reqwest::Client::new()
                    .post("https://api.anthropic.com/v1/messages")
                    .build()
                    .unwrap(),
                &anthropic_target(None),
            )
            .await
            .unwrap();
        assert_eq!(bearer(first).as_deref(), Some("Bearer first"));
        // Claude Code rotates its stored token.
        std::fs::write(
            &creds,
            r#"{"claudeAiOauth":{"accessToken":"second","refreshToken":"r"}}"#,
        )
        .unwrap();
        let second = applier
            .apply(
                reqwest::Client::new()
                    .post("https://api.anthropic.com/v1/messages")
                    .build()
                    .unwrap(),
                &anthropic_target(None),
            )
            .await
            .unwrap();
        assert_eq!(
            bearer(second).as_deref(),
            Some("Bearer second"),
            "a non-expiring marker token must be re-read live, not served from a stale cache"
        );
    }

    #[tokio::test]
    async fn api_key_prepare_body_leaves_system_untouched() {
        let path = tmp_store_path();
        {
            let mut store = CredentialStore::load(&path).unwrap();
            store
                .set(
                    PROVIDER_ID,
                    DEFAULT_LABEL,
                    Credential::api_key("sk-ant-api03-x"),
                )
                .unwrap();
        }
        let applier = AnthropicOAuthApplier::new(&path).unwrap();
        let mut body = serde_json::json!({ "system": "user prompt", "messages": [] });
        applier
            .prepare_body(&mut body, &anthropic_target(None))
            .await
            .unwrap();
        assert_eq!(body["system"], serde_json::json!("user prompt"));
    }
}
