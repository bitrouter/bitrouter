//! `ProviderEntry` — the compiled-in description of one built-in provider.
//!
//! Each entry is the source-of-truth for "how do we talk to this provider":
//! - which env vars hold the credential,
//! - which header the credential goes into,
//! - which wire protocol the provider serves (or a glob → protocol map for
//!   mixed-protocol providers like opencode-zen),
//! - the default `api_base`.
//!
//! Entries are authored as TOML under `providers/*.toml` and embedded into
//! the binary via `include_str!` (see `crate::builtin`).
//!
//! Model metadata (pricing, context length, modalities) is **not** in here —
//! that comes from <https://models.dev/api.json>, fetched + cached at
//! runtime. Mixing the two would couple every pricing change to a binary
//! release.

use std::collections::BTreeMap;

use serde::Deserialize;

use bitrouter_sdk::language_model::types::ApiProtocol;

/// One built-in provider entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    /// Provider id (the lookup key, matches `models.dev`'s provider key).
    pub id: String,
    /// Human-readable name (display only).
    pub display_name: String,
    /// Default upstream `api_base`. May be overridden by a user-written
    /// provider config.
    pub api_base: String,
    /// The wire protocol the provider serves. Either a single protocol (most
    /// providers) or a glob → protocol map (opencode-zen-shape: same provider,
    /// different protocols per model id).
    pub api_protocol: ProtocolMapping,
    /// Authentication scheme (how the credential is placed on each request).
    pub auth: AuthScheme,
    /// Link to the provider's official API documentation. Required so future
    /// readers (and reviewers of this file) can verify the auth + URL shape.
    pub doc_url: String,
}

/// Either a single protocol, or a glob → protocol map for providers that
/// serve different protocols on different model ids (e.g. opencode-zen).
///
/// `#[serde(untagged)]` so the TOML can write either:
/// ```toml
/// api_protocol = "chat_completions"
/// ```
/// or
/// ```toml
/// [api_protocol]
/// "claude-*" = "messages"
/// "*" = "chat_completions"
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ProtocolMapping {
    /// Same protocol for every model.
    Single(ApiProtocol),
    /// Glob-prefix matched protocol per model id. Longest match wins.
    PerModel(BTreeMap<String, ApiProtocol>),
}

impl ProtocolMapping {
    /// Resolve the protocol for one model id. Returns `None` if no pattern
    /// matches (callers fall back to a sensible default).
    pub fn resolve(&self, model_id: &str) -> Option<ApiProtocol> {
        match self {
            ProtocolMapping::Single(api_protocol) => Some(api_protocol.clone()),
            ProtocolMapping::PerModel(api_protocol) => {
                let mut best: Option<(&str, &ApiProtocol)> = None;
                for (pat, proto) in api_protocol {
                    if pattern_matches(pat, model_id)
                        && best.as_ref().is_none_or(|(b, _)| pat.len() > b.len())
                    {
                        best = Some((pat.as_str(), proto));
                    }
                }
                best.map(|(_, p)| p.clone())
            }
        }
    }
}

/// Trailing-`*` glob match. Anything else is an exact match. (The wildcard
/// form is the only one we need for opencode-zen-shape providers; deeper
/// glob support is in [`bitrouter_sdk::config::Pattern`] for user-written
/// patterns and we don't want to depend on it here.)
fn pattern_matches(pat: &str, model_id: &str) -> bool {
    if let Some(prefix) = pat.strip_suffix('*') {
        model_id.starts_with(prefix)
    } else {
        pat == model_id
    }
}

/// How the credential is placed on each upstream request.
///
/// Bearer + Header cover the static-credential providers (everything that
/// "set `${PROVIDER}_API_KEY` and you're done"). OAuth and SigV4 are
/// referenced by name and resolved at register-time to a handler in another
/// crate — they do not live in TOML data.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthScheme {
    /// `Authorization: Bearer <env(var)>`.
    Bearer {
        /// Env var holding the API key.
        env: String,
    },
    /// `<header>: <env(var)>` plus any constant `extra_headers`. Used by
    /// Anthropic (`x-api-key` + `anthropic-version`) and by Gemini
    /// (`x-goog-api-key`).
    Header {
        /// HTTP header carrying the credential.
        header: String,
        /// Env var holding the credential.
        env: String,
        /// Constant headers to add alongside the credential header (typically
        /// API version pins).
        #[serde(default)]
        extra_headers: BTreeMap<String, String>,
    },
    /// OAuth flow. The `handler` string names a registered handler in another
    /// crate (e.g. `"github_copilot_device_code"` resolved by a future
    /// `bitrouter-providers` `oauth` module). `params` is provider-specific
    /// freeform config (client_id, scopes, token endpoints, …).
    Oauth {
        /// Named handler in the runtime OAuth registry.
        handler: String,
        /// Handler-specific parameters (client_id, scopes, …).
        #[serde(default)]
        params: BTreeMap<String, toml::Value>,
    },
    /// SigV4 / native SDK-driven auth (e.g. Bedrock). The `handler` string
    /// names a Transport registered by an outboard crate; no credentials live
    /// in the TOML at all.
    Native {
        /// Named handler (e.g. `"aws_sigv4"`).
        handler: String,
    },
}

impl AuthScheme {
    /// The env var this scheme reads, if it reads exactly one. Returns `None`
    /// for OAuth / Native — those resolve credentials through their handler.
    /// Used by `bitrouter doctor` to report missing env vars.
    pub fn env_var(&self) -> Option<&str> {
        match self {
            AuthScheme::Bearer { env } | AuthScheme::Header { env, .. } => Some(env.as_str()),
            AuthScheme::Oauth { .. } | AuthScheme::Native { .. } => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_bearer() {
        let src = r#"
            id = "openai"
            display_name = "OpenAI"
            api_base = "https://api.openai.com/v1"
            api_protocol = "chat_completions"
            doc_url = "https://platform.openai.com/docs/api-reference"

            [auth]
            kind = "bearer"
            env = "OPENAI_API_KEY"
        "#;
        let entry: ProviderEntry = toml::from_str(src).unwrap();
        assert_eq!(entry.id, "openai");
        assert!(matches!(entry.auth, AuthScheme::Bearer { .. }));
        assert_eq!(entry.auth.env_var(), Some("OPENAI_API_KEY"));
        assert_eq!(
            entry.api_protocol.resolve("gpt-4o"),
            Some(ApiProtocol::ChatCompletions)
        );
    }

    #[test]
    fn parses_header_with_extras() {
        let src = r#"
            id = "anthropic"
            display_name = "Anthropic"
            api_base = "https://api.anthropic.com/v1"
            api_protocol = "messages"
            doc_url = "https://docs.anthropic.com/en/api/messages"

            [auth]
            kind = "header"
            header = "x-api-key"
            env = "ANTHROPIC_API_KEY"
            [auth.extra_headers]
            "anthropic-version" = "2023-06-01"
        "#;
        let entry: ProviderEntry = toml::from_str(src).unwrap();
        match &entry.auth {
            AuthScheme::Header {
                header,
                env,
                extra_headers,
            } => {
                assert_eq!(header, "x-api-key");
                assert_eq!(env, "ANTHROPIC_API_KEY");
                assert_eq!(
                    extra_headers.get("anthropic-version").map(String::as_str),
                    Some("2023-06-01")
                );
            }
            other => panic!("expected Header, got {other:?}"),
        }
    }

    #[test]
    fn parses_per_model_protocol_map() {
        // opencode-zen-shape: claude-* → messages, default → chat_completions.
        let src = r#"
            id = "opencode-zen"
            display_name = "opencode zen"
            api_base = "https://zen.opencode.example/v1"
            doc_url = "https://example.com"

            [api_protocol]
            "claude-*" = "messages"
            "*" = "chat_completions"

            [auth]
            kind = "bearer"
            env = "OPENCODE_ZEN_API_KEY"
        "#;
        let entry: ProviderEntry = toml::from_str(src).unwrap();
        assert_eq!(
            entry.api_protocol.resolve("claude-opus-4-1"),
            Some(ApiProtocol::Messages)
        );
        assert_eq!(
            entry.api_protocol.resolve("gpt-5-mini"),
            Some(ApiProtocol::ChatCompletions)
        );
    }

    #[test]
    fn rejects_unknown_fields() {
        // Catches typos like `apt_base` → instead of silently defaulting to
        // an empty string we want a parse error at startup.
        let src = r#"
            id = "x"
            display_name = "X"
            apt_base = "https://x.example/v1"
            api_protocol = "chat_completions"
            doc_url = "https://x.example"
            [auth]
            kind = "bearer"
            env = "X_API_KEY"
        "#;
        assert!(toml::from_str::<ProviderEntry>(src).is_err());
    }
}
