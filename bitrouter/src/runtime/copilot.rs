//! GitHub Copilot provider-specific request configuration.
//!
//! The Copilot API requires additional headers beyond the standard OpenAI or
//! Anthropic protocol. This module builds those headers and determines
//! protocol overrides for Copilot-hosted models.

use std::collections::HashMap;

/// The built-in provider name used for GitHub Copilot.
pub const COPILOT_PROVIDER: &str = "github-copilot";

/// Copilot request headers injected for every request to the
/// `github-copilot` provider.
///
/// These headers are merged into the provider's `default_headers` before the
/// model config is built so they arrive on every outgoing HTTP request.
pub fn copilot_default_headers() -> HashMap<String, String> {
    let mut headers = HashMap::new();
    headers.insert(
        "User-Agent".to_owned(),
        format!("bitrouter/{}", env!("CARGO_PKG_VERSION")),
    );
    headers.insert("Openai-Intent".to_owned(), "conversation-edits".to_owned());
    headers
}

/// Returns `true` when a model ID should use the Anthropic Messages API
/// instead of the default OpenAI Chat Completions API.
///
/// The Copilot API uses different protocols per model family: Claude models
/// use the Anthropic Messages API (`POST /v1/messages`), while all other
/// models use OpenAI Chat Completions (`POST /chat/completions`).
pub fn is_anthropic_model(model_id: &str) -> bool {
    model_id.starts_with("claude-") || model_id.starts_with("claude_")
}

/// Derives the Anthropic-compatible API base from the provider's base URL.
///
/// The Copilot API for Anthropic models expects the base to include `/v1`
/// (e.g. `https://api.githubcopilot.com/v1`), because the Anthropic adapter
/// appends `/v1/messages` itself.
pub fn anthropic_api_base(provider_api_base: Option<&str>) -> String {
    let base = provider_api_base.unwrap_or("https://api.githubcopilot.com");
    base.trim_end_matches('/').to_owned()
}

/// Derive OAuth endpoint URLs from a GitHub Enterprise domain.
///
/// Returns `(device_auth_url, token_url, api_base)` for the given domain.
/// For `github.com` (the default), the standard public endpoints are returned.
#[cfg(any(feature = "cli", test))]
pub fn enterprise_urls(domain: &str) -> (String, String, String) {
    if domain == "github.com" || domain.is_empty() {
        return (
            "https://github.com/login/device/code".to_owned(),
            "https://github.com/login/oauth/access_token".to_owned(),
            "https://api.githubcopilot.com".to_owned(),
        );
    }
    let domain = domain.trim_end_matches('/');
    (
        format!("https://{domain}/login/device/code"),
        format!("https://{domain}/login/oauth/access_token"),
        format!("https://copilot-api.{domain}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn copilot_headers_include_user_agent_and_intent() {
        let headers = copilot_default_headers();
        assert!(
            headers
                .get("User-Agent")
                .is_some_and(|v| v.starts_with("bitrouter/"))
        );
        assert_eq!(headers.get("Openai-Intent").unwrap(), "conversation-edits");
    }

    #[test]
    fn claude_models_detected_as_anthropic() {
        assert!(is_anthropic_model("claude-sonnet-4.6"));
        assert!(is_anthropic_model("claude-haiku-4.5"));
        assert!(is_anthropic_model("claude-opus-4.6"));
        assert!(!is_anthropic_model("gpt-5.4"));
        assert!(!is_anthropic_model("gemini-2.5-pro"));
    }

    #[test]
    fn anthropic_base_url_no_double_v1() {
        let base = anthropic_api_base(Some("https://api.githubcopilot.com"));
        assert_eq!(base, "https://api.githubcopilot.com");

        let base = anthropic_api_base(Some("https://api.githubcopilot.com/v1"));
        assert_eq!(base, "https://api.githubcopilot.com/v1");
    }

    #[test]
    fn enterprise_urls_github_com() {
        let (device, token, api) = enterprise_urls("github.com");
        assert_eq!(device, "https://github.com/login/device/code");
        assert_eq!(token, "https://github.com/login/oauth/access_token");
        assert_eq!(api, "https://api.githubcopilot.com");
    }

    #[test]
    fn enterprise_urls_custom_domain() {
        let (device, token, api) = enterprise_urls("company.ghe.com");
        assert_eq!(device, "https://company.ghe.com/login/device/code");
        assert_eq!(token, "https://company.ghe.com/login/oauth/access_token");
        assert_eq!(api, "https://copilot-api.company.ghe.com");
    }
}
