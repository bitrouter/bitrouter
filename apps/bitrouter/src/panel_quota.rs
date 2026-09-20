//! Bounded, daemon-owned upstream quota cache for the menu-bar panel.

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};

use crate::account_ref::AccountRefKey;
use crate::actions::panel::{PanelQuota, QuotaWindow};

const CODEX_TTL_SECS: u64 = 60;

#[derive(Clone)]
/// Process-local cache and scheduler for passive upstream quota reads.
pub struct PanelQuotaService {
    inner: Arc<PanelQuotaInner>,
}

struct PanelQuotaInner {
    key: AccountRefKey,
    store_path: PathBuf,
    client: reqwest::Client,
    usage_url: String,
    cache: Mutex<HashMap<String, CacheEntry>>,
}

#[derive(Clone)]
struct CacheEntry {
    quota: PanelQuota,
    expires_at: Instant,
    in_flight: bool,
}

#[derive(Clone)]
struct ResolvedCredential {
    account_ref: String,
    token: bitrouter_providers::oauth::credential_store::OAuthToken,
}

impl PanelQuotaService {
    /// Build a service over the exact credential store used by the daemon.
    pub fn new(key: AccountRefKey, store_path: PathBuf) -> anyhow::Result<Self> {
        let client = reqwest::Client::builder()
            .timeout(Duration::from_secs(15))
            .redirect(reqwest::redirect::Policy::none())
            .user_agent(concat!("bitrouter/", env!("CARGO_PKG_VERSION")))
            .build()?;
        Ok(Self {
            inner: Arc::new(PanelQuotaInner {
                key,
                store_path,
                client,
                usage_url: "https://chatgpt.com/backend-api/wham/usage".into(),
                cache: Mutex::new(HashMap::new()),
            }),
        })
    }

    #[cfg(test)]
    fn with_usage_url(mut self, usage_url: String) -> Self {
        if let Some(inner) = Arc::get_mut(&mut self.inner) {
            inner.usage_url = usage_url;
        }
        self
    }

    /// Return cached results immediately and schedule at most one refresh per
    /// current account. Credentials that disappeared invalidate old samples.
    pub fn snapshot(&self, account_refs: &[String]) -> HashMap<String, PanelQuota> {
        let credentials = self.resolve_codex_credentials();
        let current = credentials
            .iter()
            .map(|credential| credential.account_ref.clone())
            .collect::<HashSet<_>>();
        let requested = account_refs.iter().cloned().collect::<HashSet<_>>();
        let now = Instant::now();
        let mut result = HashMap::new();
        let mut refresh = Vec::new();
        if let Ok(mut cache) = self.inner.cache.lock() {
            cache.retain(|account_ref, _| current.contains(account_ref));
            for account_ref in requested {
                let Some(credential) = credentials
                    .iter()
                    .find(|credential| credential.account_ref == account_ref)
                else {
                    result.insert(account_ref, unavailable("credential_unavailable"));
                    continue;
                };
                let entry = cache
                    .entry(account_ref.clone())
                    .or_insert_with(|| CacheEntry {
                        quota: pending(),
                        expires_at: now,
                        in_flight: false,
                    });
                result.insert(account_ref.clone(), entry.quota.clone());
                if now >= entry.expires_at && !entry.in_flight {
                    entry.in_flight = true;
                    if entry.quota.state == "available" {
                        entry.quota.state = "stale".into();
                        result.insert(account_ref.clone(), entry.quota.clone());
                    }
                    refresh.push(credential.clone());
                }
            }
        }
        for credential in refresh {
            let inner = Arc::clone(&self.inner);
            tokio::spawn(async move {
                let (quota, ttl_secs) =
                    fetch_codex(&inner.client, &inner.usage_url, &credential.token).await;
                if let Ok(mut cache) = inner.cache.lock()
                    && let Some(entry) = cache.get_mut(&credential.account_ref)
                {
                    if quota.state == "error"
                        && matches!(entry.quota.state.as_str(), "available" | "stale")
                    {
                        entry.quota.state = "stale".into();
                        entry.quota.error = quota.error;
                    } else {
                        entry.quota = quota;
                    }
                    let now = Instant::now();
                    entry.expires_at = match now.checked_add(ttl_secs) {
                        Some(expires_at) => expires_at,
                        None => now + Duration::from_secs(300),
                    };
                    entry.in_flight = false;
                }
            });
        }
        result
    }

    fn resolve_codex_credentials(&self) -> Vec<ResolvedCredential> {
        let Ok(store) = bitrouter_providers::oauth::credential_store::CredentialStore::load(
            &self.inner.store_path,
        ) else {
            return Vec::new();
        };
        store
            .labels(bitrouter_providers::codex::PROVIDER_ID)
            .into_iter()
            .filter_map(|label| {
                let credential = store
                    .get(bitrouter_providers::codex::PROVIDER_ID, label)?
                    .as_oauth()?
                    .clone();
                let authority =
                    bitrouter_providers::codex::OpenAiCodexAuthApplier::credential_authority(
                        &credential,
                    )?;
                let account_ref = self.inner.key.derive(
                    bitrouter_providers::codex::PROVIDER_ID,
                    &bitrouter_sdk::language_model::auth::ContinuationAuthority::new(
                        authority,
                        bitrouter_sdk::language_model::types::AuthScheme::Bearer,
                    ),
                )?;
                Some(ResolvedCredential {
                    account_ref,
                    token: credential,
                })
            })
            .collect()
    }
}

async fn fetch_codex(
    client: &reqwest::Client,
    usage_url: &str,
    token: &bitrouter_providers::oauth::credential_store::OAuthToken,
) -> (PanelQuota, Duration) {
    let Ok(claims) = bitrouter_providers::codex::jwt::decode_codex_claims(&token.access_token)
    else {
        return (
            unavailable("account_identity_unavailable"),
            Duration::from_secs(300),
        );
    };
    let Some(account_id) = claims.chatgpt_account_id.filter(|value| !value.is_empty()) else {
        return (
            unavailable("account_identity_unavailable"),
            Duration::from_secs(300),
        );
    };
    let response = client
        .get(usage_url)
        .bearer_auth(&token.access_token)
        .header("chatgpt-account-id", account_id)
        .send()
        .await;
    let Ok(response) = response else {
        return (failed("quota_transport_failed"), Duration::from_secs(300));
    };
    if !response.status().is_success() {
        let retry = response
            .headers()
            .get(reqwest::header::RETRY_AFTER)
            .and_then(|value| value.to_str().ok())
            .and_then(|value| value.parse::<u64>().ok())
            .filter(|seconds| *seconds > 0)
            .map(Duration::from_secs)
            .filter(|duration| Instant::now().checked_add(*duration).is_some())
            .unwrap_or(Duration::from_secs(300));
        return (
            failed(match response.status().as_u16() {
                401 | 403 => "quota_auth_failed",
                429 => "quota_rate_limited",
                _ => "quota_upstream_failed",
            }),
            retry,
        );
    }
    if response
        .content_length()
        .is_some_and(|length| length > 1_048_576)
    {
        return (failed("quota_response_too_large"), Duration::from_secs(300));
    }
    let mut response = response;
    let mut bytes = Vec::new();
    loop {
        let chunk = match response.chunk().await {
            Ok(Some(chunk)) => chunk,
            Ok(None) => break,
            Err(_) => {
                return (
                    failed("quota_response_read_failed"),
                    Duration::from_secs(300),
                );
            }
        };
        if bytes.len().saturating_add(chunk.len()) > 1_048_576 {
            return (failed("quota_response_too_large"), Duration::from_secs(300));
        }
        bytes.extend_from_slice(&chunk);
    }
    let Ok(payload) = serde_json::from_slice::<serde_json::Value>(&bytes) else {
        return (failed("quota_invalid_response"), Duration::from_secs(300));
    };
    (
        parse_codex_payload(&payload),
        Duration::from_secs(CODEX_TTL_SECS),
    )
}

fn parse_codex_payload(payload: &serde_json::Value) -> PanelQuota {
    let mut windows = Vec::new();
    append_windows(&mut windows, "Codex", payload.get("rate_limit"));
    if let Some(additional) = payload
        .get("additional_rate_limits")
        .and_then(serde_json::Value::as_array)
    {
        for (index, limit) in additional.iter().enumerate() {
            let scope = limit
                .get("limit_name")
                .and_then(serde_json::Value::as_str)
                .filter(|value| !value.is_empty())
                .or_else(|| {
                    limit
                        .get("metered_feature")
                        .and_then(serde_json::Value::as_str)
                        .filter(|value| !value.is_empty())
                })
                .map(ToOwned::to_owned)
                .unwrap_or_else(|| format!("Additional limit {}", index + 1));
            append_windows(&mut windows, &scope, limit.get("rate_limit"));
        }
    }
    if windows.is_empty() {
        return failed("quota_windows_unavailable");
    }
    PanelQuota {
        state: "available".into(),
        sampled_at: Some(Utc::now()),
        error: None,
        windows,
    }
}

fn append_windows(windows: &mut Vec<QuotaWindow>, scope: &str, limits: Option<&serde_json::Value>) {
    let limits = limits.and_then(serde_json::Value::as_object);
    for (key, fallback) in [
        ("primary_window", "Primary"),
        ("secondary_window", "Secondary"),
    ] {
        let Some(window) = limits.and_then(|limits| limits.get(key)) else {
            continue;
        };
        let Some(window) = window.as_object() else {
            continue;
        };
        let Some(used) = window
            .get("used_percent")
            .and_then(serde_json::Value::as_f64)
            .filter(|value| (0.0..=100.0).contains(value))
        else {
            continue;
        };
        let duration = window
            .get("limit_window_seconds")
            .and_then(serde_json::Value::as_u64);
        windows.push(QuotaWindow {
            label: format!(
                "{scope} · {} · {}",
                duration.map(window_label).unwrap_or("Usage window"),
                fallback.to_lowercase()
            ),
            remaining_percent: Some(100.0 - used),
            remaining_tokens: None,
            remaining_requests: None,
            remaining_currency: None,
            currency: None,
            resets_at: window
                .get("reset_at")
                .and_then(serde_json::Value::as_i64)
                .and_then(|seconds| DateTime::from_timestamp(seconds, 0)),
            reset_kind: Some("unknown".into()),
        });
    }
}

fn window_label(seconds: u64) -> &'static str {
    match seconds {
        18_000 => "5 hours",
        604_800 => "Weekly",
        _ => "Usage window",
    }
}

fn pending() -> PanelQuota {
    PanelQuota {
        state: "unknown".into(),
        sampled_at: None,
        error: None,
        windows: Vec::new(),
    }
}

fn unavailable(error: &str) -> PanelQuota {
    PanelQuota {
        state: "error".into(),
        sampled_at: None,
        error: Some(error.into()),
        windows: Vec::new(),
    }
}

fn failed(error: &str) -> PanelQuota {
    PanelQuota {
        state: "error".into(),
        sampled_at: Some(Utc::now()),
        error: Some(error.into()),
        windows: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use base64::Engine;
    use wiremock::matchers::{header_exists, method, path};
    use wiremock::{Mock, MockServer, ResponseTemplate};

    #[test]
    fn parser_preserves_missing_and_rejects_empty_windows() {
        let empty = parse_codex_payload(&serde_json::json!({"rate_limit": {}}));
        assert_eq!(empty.state, "error");
        assert!(empty.windows.is_empty());

        let parsed = parse_codex_payload(&serde_json::json!({
            "rate_limit": {
                "primary_window": {
                    "used_percent": 12.5,
                    "limit_window_seconds": 604800,
                    "reset_at": 1_800_000_000
                },
                "secondary_window": null
            },
            "additional_rate_limits": [{
                "limit_name": "Codex Spark",
                "metered_feature": "codex_spark",
                "rate_limit": {"primary_window": {
                    "used_percent": 10,
                    "limit_window_seconds": 604800
                }}
            }]
        }));
        assert_eq!(parsed.state, "available");
        assert_eq!(parsed.windows.len(), 2);
        assert_eq!(parsed.windows[0].remaining_percent, Some(87.5));
        assert_eq!(parsed.windows[0].label, "Codex · Weekly · primary");
        assert_eq!(parsed.windows[1].label, "Codex Spark · Weekly · primary");
    }

    #[test]
    fn parser_never_turns_invalid_percent_into_zero() {
        let parsed = parse_codex_payload(&serde_json::json!({
            "rate_limit": {"primary_window": {"used_percent": 120}}
        }));
        assert_eq!(parsed.state, "error");
        assert!(parsed.windows.is_empty());
    }

    fn jwt(account: &str) -> String {
        let encode = |value: &[u8]| base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(value);
        let claims = serde_json::json!({
            "exp": 4_000_000_000_u64,
            "https://api.openai.com/auth": {"chatgpt_account_id": account}
        });
        format!(
            "{}.{}.{}",
            encode(b"{}"),
            encode(
                serde_json::to_string(&claims)
                    .as_deref()
                    .unwrap_or("{}")
                    .as_bytes()
            ),
            encode(b"sig")
        )
    }

    #[tokio::test]
    async fn coalesces_refresh_and_invalidates_removed_credential() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let store_path = directory.path().join("oauth-tokens.json");
        let token = bitrouter_providers::oauth::credential_store::OAuthToken {
            access_token: jwt("account-a"),
            expires_at: 4_000_000_000,
            refresh_token: Some("refresh-a".into()),
        };
        let mut store =
            bitrouter_providers::oauth::credential_store::CredentialStore::load(&store_path)?;
        store.set(
            bitrouter_providers::codex::PROVIDER_ID,
            "default",
            bitrouter_providers::oauth::credential_store::Credential::from_oauth_token(
                token.clone(),
            ),
        )?;
        let server = MockServer::start().await;
        Mock::given(method("GET"))
            .and(path("/usage"))
            .and(header_exists("authorization"))
            .respond_with(ResponseTemplate::new(200).set_body_json(serde_json::json!({
                "rate_limit": {"primary_window": {
                    "used_percent": 25,
                    "limit_window_seconds": 604800
                }}
            })))
            .expect(2)
            .mount(&server)
            .await;
        let key = AccountRefKey::from_bytes([9; 32]);
        let authority =
            bitrouter_providers::codex::OpenAiCodexAuthApplier::credential_authority(&token)
                .ok_or_else(|| anyhow::anyhow!("fixture account claim missing"))?;
        let account_ref = key
            .derive(
                bitrouter_providers::codex::PROVIDER_ID,
                &bitrouter_sdk::language_model::auth::ContinuationAuthority::new(
                    authority,
                    bitrouter_sdk::language_model::types::AuthScheme::Bearer,
                ),
            )
            .ok_or_else(|| anyhow::anyhow!("fixture ref derivation failed"))?;
        let service = PanelQuotaService::new(key, store_path.clone())?
            .with_usage_url(format!("{}/usage", server.uri()));

        let first = service.snapshot(std::slice::from_ref(&account_ref));
        assert_eq!(first[&account_ref].state, "unknown");
        let second = service.snapshot(std::slice::from_ref(&account_ref));
        assert_eq!(second[&account_ref].state, "unknown");
        let available = tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = service.snapshot(std::slice::from_ref(&account_ref));
                if snapshot[&account_ref].state == "available" {
                    break snapshot[&account_ref].clone();
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;

        if let Ok(mut cache) = service.inner.cache.lock()
            && let Some(entry) = cache.get_mut(&account_ref)
        {
            entry.expires_at = Instant::now()
                .checked_sub(Duration::from_secs(1))
                .ok_or_else(|| anyhow::anyhow!("fixture instant underflow"))?;
        }
        let expired = service.snapshot(std::slice::from_ref(&account_ref));
        assert_eq!(expired[&account_ref].state, "stale");
        assert_eq!(expired[&account_ref].sampled_at, available.sampled_at);
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                let snapshot = service.snapshot(std::slice::from_ref(&account_ref));
                if snapshot[&account_ref].state == "available" {
                    break;
                }
                tokio::task::yield_now().await;
            }
        })
        .await?;

        let mut store =
            bitrouter_providers::oauth::credential_store::CredentialStore::load(&store_path)?;
        store.remove_all_for(bitrouter_providers::codex::PROVIDER_ID)?;
        let removed = service.snapshot(std::slice::from_ref(&account_ref));
        assert_eq!(removed[&account_ref].state, "error");
        assert_eq!(
            removed[&account_ref].error.as_deref(),
            Some("credential_unavailable")
        );
        Ok(())
    }
}
