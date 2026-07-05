//! Per-provider PKCE OAuth registrations.
//!
//! Each entry pairs a bitrouter provider id with the public OAuth client
//! the corresponding vendor CLI ships with, plus the bound loopback port
//! and any provider-specific `/authorize` extras.
//!
//! These values are **not in the vendor's published docs** — they're the
//! widely-used public client ids that ship inside open-source clients
//! (Claude Code's OAuth client, OpenAI's Codex CLI) and reused by
//! third-party agents (OpenCode, OpenClaw, …) talking to the same
//! subscription endpoints. Bitrouter follows the same convention. If a
//! vendor rotates a client id, update it here.

use std::collections::BTreeMap;

use crate::oauth::auth_code::AuthCodeParams;

/// One registered PKCE provider — the OAuth client config + which loopback
/// port to bind for the redirect.
#[derive(Debug, Clone)]
pub struct PkceProvider {
    /// Bitrouter provider id (e.g. `anthropic`, `openai-codex`). Used both
    /// as the lookup key here and as the [`crate::oauth::credential_store`]
    /// key the resulting credential is stored under.
    pub provider_id: &'static str,
    /// Human label for `bitrouter providers login` output (e.g. "Claude
    /// Pro/Max subscription").
    pub display_name: &'static str,
    /// Local TCP port to bind for the redirect listener. `None` → ask the
    /// OS for any free port (use only when the upstream tolerates a dynamic
    /// localhost port).
    pub loopback_port: Option<u16>,
    /// Path portion of the redirect URI (e.g. `/auth/callback`).
    pub redirect_path: &'static str,
    /// Manual-paste fallback URI to present when the loopback listener
    /// can't be bound (port already taken, headless host, …). The user
    /// pastes the full redirect URL after the vendor redirects them here.
    /// `None` → manual fallback is not supported for this provider.
    pub manual_redirect_uri: Option<&'static str>,
    /// Underlying OAuth client params — endpoints, scope, extras.
    pub auth: AuthCodeParams,
}

/// Look up the PKCE registration for `provider_id`. Returns `None` when
/// the provider doesn't have a PKCE login configured (which is most of
/// them — only `anthropic` and `openai-codex` ship with one today).
pub fn find(provider_id: &str) -> Option<PkceProvider> {
    match provider_id {
        "anthropic" => Some(anthropic()),
        "openai-codex" => Some(openai_codex()),
        _ => None,
    }
}

/// Whether `provider_id` has a PKCE login registered.
pub fn has_pkce_flow(provider_id: &str) -> bool {
    find(provider_id).is_some()
}

/// All registered PKCE providers — useful for `bitrouter providers login` UX
/// that wants to list every supported subscription flow.
pub fn all() -> Vec<PkceProvider> {
    vec![anthropic(), openai_codex()]
}

/// Anthropic — Claude Pro/Max subscription OAuth.
///
/// Endpoints and client id are the ones Claude Code uses; they appear in
/// many third-party clients (OpenClaw via `@earendil-works/pi-ai`,
/// OpenCode's auth registry, etc.). Scopes mirror what Claude Code
/// requests.
///
/// The loopback redirect uses an OS-assigned port (`None`) — Anthropic's
/// OAuth accepts any `http://localhost:*` redirect URI by design (the
/// public-client model for native CLIs).
fn anthropic() -> PkceProvider {
    let mut extra = BTreeMap::new();
    // Claude Code sends `code=true` on the authorize request; mirror it so the
    // consent flow returns an authorization code for both the loopback and the
    // manual-paste paths. (Reference: OpenClaw `src/llm/utils/oauth/anthropic.ts`.)
    extra.insert("code".into(), "true".into());
    PkceProvider {
        provider_id: "anthropic",
        display_name: "Anthropic Claude Pro/Max",
        loopback_port: None,
        redirect_path: "/callback",
        // Manual paste flow lands users at Anthropic's console
        // OAuth-code page, which shows `code#state` for the user to copy.
        manual_redirect_uri: Some("https://console.anthropic.com/oauth/code/callback"),
        auth: AuthCodeParams {
            client_id: "9d1c250a-e61b-44d9-88ed-5944d1962f5e".into(),
            authorize_endpoint: "https://claude.ai/oauth/authorize".into(),
            // Claude Code's OAuth token endpoint. `platform.claude.com` is the
            // current canonical host (the older `console.anthropic.com` alias
            // still resolves). Used for the initial code exchange and refresh.
            token_endpoint: "https://platform.claude.com/v1/oauth/token".into(),
            // Scopes Claude Code requests. `user:inference` authorises the
            // subscription inference calls; `user:sessions:claude_code` gates
            // the Claude Code agent profile the OAuth path runs under.
            // (Reference: OpenClaw `src/llm/utils/oauth/anthropic.ts`.)
            scope: "org:create_api_key user:profile user:inference \
                    user:sessions:claude_code user:mcp_servers user:file_upload"
                .into(),
            extra_authorize: extra,
        },
    }
}

/// OpenAI Codex — ChatGPT Plus/Pro subscription OAuth.
///
/// Client id `app_EMoamEEZ73f0CkXaXp7hrann` is OpenAI Codex CLI's public
/// client (confirmed in OpenCode's `plugin/codex.ts` and OpenClaw's
/// `extensions/openai/openai-codex-device-code.ts`). The loopback port is
/// pinned to `1455` because the OAuth client is registered against that
/// exact callback URL — dynamic ports get rejected.
///
/// The extra authorize params `id_token_add_organizations=true` and
/// `codex_cli_simplified_flow=true` are what the official Codex CLI
/// sends; they trigger OpenAI's enrich-the-id-token path so the
/// returned JWT carries `chatgpt_account_id` for [`crate::codex`] to
/// pull out and forward as the `chatgpt-account-id` request header.
fn openai_codex() -> PkceProvider {
    let mut extra = BTreeMap::new();
    extra.insert("id_token_add_organizations".into(), "true".into());
    extra.insert("codex_cli_simplified_flow".into(), "true".into());
    // Tag the authorize step with the same `originator` we send on inference
    // requests so login and traffic are attributed consistently. (Reference:
    // OpenClaw `extensions/openai/openai-chatgpt-oauth-flow.runtime.ts`.)
    extra.insert(
        "originator".into(),
        crate::codex::headers::ORIGINATOR.to_string(),
    );
    PkceProvider {
        provider_id: "openai-codex",
        display_name: "OpenAI Codex (ChatGPT Plus/Pro)",
        loopback_port: Some(1455),
        redirect_path: "/auth/callback",
        manual_redirect_uri: None,
        auth: AuthCodeParams {
            client_id: "app_EMoamEEZ73f0CkXaXp7hrann".into(),
            authorize_endpoint: "https://auth.openai.com/oauth/authorize".into(),
            token_endpoint: "https://auth.openai.com/oauth/token".into(),
            scope: "openid profile email offline_access".into(),
            extra_authorize: extra,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lookup_returns_known_providers() {
        assert!(find("anthropic").is_some());
        assert!(find("openai-codex").is_some());
        assert!(find("openai").is_none());
        assert!(find("github-copilot").is_none()); // device-code, not PKCE
    }

    #[test]
    fn has_pkce_flow_matches_find() {
        assert!(has_pkce_flow("anthropic"));
        assert!(has_pkce_flow("openai-codex"));
        assert!(!has_pkce_flow("openai"));
    }

    #[test]
    fn anthropic_endpoints_are_https() {
        let p = find("anthropic").unwrap();
        assert!(p.auth.authorize_endpoint.starts_with("https://"));
        assert!(p.auth.token_endpoint.starts_with("https://"));
    }

    #[test]
    fn openai_codex_pins_port_1455() {
        let p = find("openai-codex").unwrap();
        assert_eq!(p.loopback_port, Some(1455));
        assert_eq!(p.redirect_path, "/auth/callback");
    }

    #[test]
    fn openai_codex_carries_id_token_extras() {
        let p = find("openai-codex").unwrap();
        assert_eq!(
            p.auth
                .extra_authorize
                .get("id_token_add_organizations")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            p.auth
                .extra_authorize
                .get("codex_cli_simplified_flow")
                .map(String::as_str),
            Some("true")
        );
        assert_eq!(
            p.auth.extra_authorize.get("originator").map(String::as_str),
            Some("bitrouter")
        );
    }

    #[test]
    fn anthropic_offers_manual_paste_fallback() {
        let p = find("anthropic").unwrap();
        assert_eq!(
            p.manual_redirect_uri,
            Some("https://console.anthropic.com/oauth/code/callback")
        );
    }

    #[test]
    fn anthropic_requests_claude_code_scopes_and_code_extra() {
        let p = find("anthropic").unwrap();
        assert_eq!(
            p.auth.token_endpoint,
            "https://platform.claude.com/v1/oauth/token"
        );
        assert!(p.auth.scope.contains("user:inference"));
        assert!(p.auth.scope.contains("user:sessions:claude_code"));
        assert_eq!(
            p.auth.extra_authorize.get("code").map(String::as_str),
            Some("true")
        );
    }

    #[test]
    fn all_includes_both_providers() {
        let names: Vec<&str> = all().iter().map(|p| p.provider_id).collect();
        assert!(names.contains(&"anthropic"));
        assert!(names.contains(&"openai-codex"));
    }
}
