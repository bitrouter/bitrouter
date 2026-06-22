//! Anthropic — the Platform-API `AuthApplier` (`x-api-key`).
//!
//! Registered under the provider id `"anthropic"`. This applier covers the
//! Anthropic **Platform API** (pay-as-you-go) only: it resolves a static API
//! key and sets `x-api-key` + `anthropic-version`. It does **no** OAuth, no
//! `ClaudeCodeCli` marker resolution, and no body shaping — the Claude Pro/Max
//! subscription path lives in the separate [`crate::claude_code`] applier
//! (provider id `"claude-code"`).
//!
//! | Source of the key | Outbound headers |
//! |---|---|
//! | `Credential::ApiKey` stored under `"anthropic"` | `x-api-key: <value>`, `anthropic-version: 2023-06-01`. |
//! | _no stored key_ | Fall back to the routing target's inline `api_key` (the `${ANTHROPIC_API_KEY}` env path). |
//! | _neither_ | `401` pointing at `ANTHROPIC_API_KEY` / `bitrouter providers login anthropic`. |
//!
//! [`AnthropicApiKeyApplier::prepare_body`] is a no-op: the Platform API takes
//! the caller's body verbatim.
//!
//! The shared header constants (`anthropic-version`, and the Claude Code
//! constants reused by [`crate::claude_code`]) live in [`headers`].

pub mod headers;

use async_trait::async_trait;
use reqwest::header::HeaderValue;

use bitrouter_sdk::language_model::AuthApplier;
use bitrouter_sdk::language_model::types::RoutingTarget;
use bitrouter_sdk::{BitrouterError, Result};

use crate::oauth::credential_store::{Credential, CredentialStore, DEFAULT_LABEL};

/// Provider id this applier is registered under.
pub const PROVIDER_ID: &str = "anthropic";

/// `AuthApplier` for `provider_name == "anthropic"` — the Anthropic Platform
/// API (`x-api-key`).
///
/// The applier owns the credential-store path so it can read a stored API key.
/// When no key is stored it falls through to the routing target's inline
/// `api_key` (the `${ANTHROPIC_API_KEY}` env path), preserving existing setups.
pub struct AnthropicApiKeyApplier {
    store_path: std::path::PathBuf,
}

impl AnthropicApiKeyApplier {
    /// Build an applier that reads the credential store at `store_path`.
    pub fn new(store_path: impl Into<std::path::PathBuf>) -> Result<Self> {
        Ok(Self {
            store_path: store_path.into(),
        })
    }

    fn label_for<'a>(&self, target: &'a RoutingTarget) -> &'a str {
        target.account_label.as_deref().unwrap_or(DEFAULT_LABEL)
    }

    /// Resolve a stored API key for the given account label, if one is present.
    /// Only [`Credential::ApiKey`] is honoured — any other shape (e.g. a stray
    /// OAuth credential) is ignored, leaving the caller to fall through to the
    /// routing target's inline key.
    fn stored_api_key(&self, label: &str) -> Result<Option<String>> {
        let store = CredentialStore::load(&self.store_path).map_err(|e| {
            BitrouterError::internal(format!(
                "reading credential store at {}: {e}",
                self.store_path.display()
            ))
        })?;
        match store.get_any(PROVIDER_ID, label) {
            Some(Credential::ApiKey { value }) => Ok(Some(value.clone())),
            _ => Ok(None),
        }
    }
}

#[async_trait]
impl AuthApplier for AnthropicApiKeyApplier {
    async fn apply(
        &self,
        mut request: reqwest::Request,
        target: &RoutingTarget,
    ) -> Result<reqwest::Request> {
        let label = self.label_for(target);
        // `anthropic-version` is mandatory.
        request.headers_mut().insert(
            "anthropic-version",
            HeaderValue::from_static(headers::ANTHROPIC_VERSION),
        );
        // Prefer a key stored under "anthropic"; otherwise fall through to the
        // routing target's inline key (the env-var path).
        let key = match self.stored_api_key(label)? {
            Some(stored) => stored,
            None => {
                let inline = target.effective_api_key();
                if inline.is_empty() {
                    return Err(BitrouterError::Upstream {
                        status: 401,
                        message: "no anthropic credential — set ANTHROPIC_API_KEY or run \
                             `bitrouter providers login anthropic`"
                            .into(),
                    });
                }
                inline.to_string()
            }
        };
        apply_api_key_header(&mut request, &key)?;
        Ok(request)
    }

    async fn prepare_body(
        &self,
        _body: &mut serde_json::Value,
        _target: &RoutingTarget,
    ) -> Result<()> {
        // The Platform API takes the caller's body verbatim — never shaped.
        Ok(())
    }
}

fn apply_api_key_header(request: &mut reqwest::Request, key: &str) -> Result<()> {
    let value = HeaderValue::from_str(key).map_err(|e| {
        BitrouterError::internal(format!("invalid api key for x-api-key header: {e}"))
    })?;
    request.headers_mut().insert("x-api-key", value);
    // Clear any stale Bearer the protocol layer might have added — the
    // Platform API authenticates via x-api-key only.
    request.headers_mut().remove(reqwest::header::AUTHORIZATION);
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use bitrouter_sdk::language_model::types::ApiProtocol;

    use super::*;

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
        let applier = AnthropicApiKeyApplier::new(&path).unwrap();
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
        let applier = AnthropicApiKeyApplier::new(&path).unwrap();
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
            msg.contains("bitrouter providers login anthropic"),
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
        let applier = AnthropicApiKeyApplier::new(&path).unwrap();
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
        let applier = AnthropicApiKeyApplier::new(&path).unwrap();
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
        let applier = AnthropicApiKeyApplier::new(&path).unwrap();
        let mut body = serde_json::json!({ "system": "user prompt", "messages": [] });
        applier
            .prepare_body(&mut body, &anthropic_target(None))
            .await
            .unwrap();
        assert_eq!(body["system"], serde_json::json!("user prompt"));
    }
}
