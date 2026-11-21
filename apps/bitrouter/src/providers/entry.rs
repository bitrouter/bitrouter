//! Provider metadata used by CLI login and application defaults.
//!
//! Entries retain credential environment variables, public login parameters,
//! protocol mappings and endpoint defaults. Runtime transports consume the
//! registry's structured authentication declaration directly.
//!
//! The Cloud defaults are embedded from `apps/bitrouter/providers/bitrouter.toml`;
//! other entries derive from registry metadata through [`crate::providers::builtin`].

use std::collections::BTreeMap;

use serde::Deserialize;

use crate::config::ProviderClass;
use bitrouter_ai::types::ProtocolList;

/// One provider's application defaults and login inputs.
#[derive(Debug, Clone, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProviderEntry {
    /// Provider id (the lookup key, matches the registry provider id).
    pub id: String,
    /// Human-readable name (display only).
    pub display_name: String,
    /// Default upstream `api_base`. May be overridden by a user-written
    /// provider config.
    pub api_base: String,
    /// The wire protocol(s) the provider serves. Either one protocol set for
    /// every model (most providers), or a glob → protocol-set map
    /// (opencode-zen-shape: same provider, different protocols per model id).
    pub api_protocol: ProtocolMapping,
    /// Optional per-protocol base-URL override (protocol name → base URL), for
    /// a provider that serves different protocols at different paths under one
    /// host — e.g. OpenAI-style under `BASE/v1` and Anthropic Messages under
    /// `BASE/anthropic`. Empty for the common single-endpoint provider.
    #[serde(default)]
    pub protocol_endpoints: BTreeMap<String, String>,
    /// Credential discovery and login inputs for this provider.
    pub auth: AuthScheme,
    /// Routing-preference class for this built-in. Applied to a configured
    /// provider's [`class`](crate::config::ProviderConfig::class) when
    /// it is unset, so gateways and the hosted cloud land in the right priority
    /// tier even though they are not registry providers. Optional — a built-in
    /// without a class stays unranked (sorts last in the auto-cascade).
    #[serde(default)]
    pub class: Option<ProviderClass>,
}

/// Either one protocol set for every model, or a glob → protocol-set map for
/// providers that serve different protocols on different model ids (e.g.
/// opencode-zen). Each value is a [`ProtocolList`] — a bare string or an
/// ordered sequence — so single- and multi-protocol upstreams share one schema.
///
/// `#[serde(untagged)]` so the TOML can write any of:
/// ```toml
/// api_protocol = "chat_completions"                  # one protocol
/// api_protocol = ["chat_completions", "responses"]   # an ordered set
/// ```
/// or per model id:
/// ```toml
/// [api_protocol]
/// "claude-*" = "messages"
/// "*" = ["chat_completions", "responses"]
/// ```
#[derive(Debug, Clone, Deserialize)]
#[serde(untagged)]
pub enum ProtocolMapping {
    /// Same protocol set for every model.
    Single(ProtocolList),
    /// Model patterns translated into the application's pattern map.
    PerModel(BTreeMap<String, ProtocolList>),
}

/// Credential discovery and public login parameters used by the application.
/// Runtime header construction and handler selection use the full registry auth.
#[derive(Debug, Clone, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AuthScheme {
    /// No credential is needed.
    None,
    /// Static bearer credential supplied by an environment variable.
    Bearer { env: String },
    /// Static header credential supplied by an environment variable.
    Header { env: String },
    /// Public OAuth login parameters, including device-code endpoints and scope.
    Oauth {
        #[serde(default)]
        params: BTreeMap<String, toml::Value>,
    },
    /// Credentials supplied by a native transport.
    Native,
}

impl AuthScheme {
    /// The env var this scheme reads, if it reads exactly one. Returns `None`
    /// for OAuth / Native — those resolve credentials through their handler.
    /// Drives [`crate::providers::apply::zero_config`]'s provider auto-enable and the provider →
    /// credential-var listing in [`crate::providers::apply::zero_config_env_var_providers`].
    pub fn env_var(&self) -> Option<&str> {
        match self {
            AuthScheme::Bearer { env } | AuthScheme::Header { env, .. } => Some(env.as_str()),
            AuthScheme::None | AuthScheme::Oauth { .. } | AuthScheme::Native => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use bitrouter_ai::types::ApiProtocol;

    #[test]
    fn parses_bearer() -> anyhow::Result<()> {
        let src = r#"
            id = "openai"
            display_name = "OpenAI"
            api_base = "https://api.openai.com/v1"
            api_protocol = "chat_completions"

            [auth]
            kind = "bearer"
            env = "OPENAI_API_KEY"
        "#;
        let entry: ProviderEntry = toml::from_str(src).context("required fixture value")?;
        assert_eq!(entry.id, "openai");
        assert!(matches!(entry.auth, AuthScheme::Bearer { .. }));
        assert_eq!(entry.auth.env_var(), Some("OPENAI_API_KEY"));
        let ProtocolMapping::Single(protocols) = &entry.api_protocol else {
            anyhow::bail!("expected one protocol set");
        };
        assert_eq!(protocols.as_slice(), &[ApiProtocol::ChatCompletions]);
        Ok(())
    }

    #[test]
    fn parses_header_credential_environment() -> anyhow::Result<()> {
        let src = r#"
            id = "anthropic"
            display_name = "Anthropic"
            api_base = "https://api.anthropic.com/v1"
            api_protocol = "messages"

            [auth]
            kind = "header"
            env = "ANTHROPIC_API_KEY"
        "#;
        let entry: ProviderEntry = toml::from_str(src).context("required fixture value")?;
        assert!(matches!(&entry.auth, AuthScheme::Header { env } if env == "ANTHROPIC_API_KEY"));
        assert_eq!(entry.auth.env_var(), Some("ANTHROPIC_API_KEY"));
        Ok(())
    }

    #[test]
    fn parses_per_model_protocol_map() -> anyhow::Result<()> {
        // opencode-zen-shape: claude-* → messages, default → chat_completions.
        let src = r#"
            id = "opencode-zen"
            display_name = "opencode zen"
            api_base = "https://zen.opencode.example/v1"

            [api_protocol]
            "claude-*" = "messages"
            "*" = "chat_completions"

            [auth]
            kind = "bearer"
            env = "OPENCODE_ZEN_API_KEY"
        "#;
        let entry: ProviderEntry = toml::from_str(src).context("required fixture value")?;
        let ProtocolMapping::PerModel(protocols) = &entry.api_protocol else {
            anyhow::bail!("expected per-model protocols");
        };
        assert_eq!(
            protocols
                .get("claude-*")
                .context("Claude protocols")?
                .as_slice(),
            &[ApiProtocol::Messages]
        );
        assert_eq!(
            protocols.get("*").context("default protocols")?.as_slice(),
            &[ApiProtocol::ChatCompletions]
        );
        Ok(())
    }

    #[test]
    fn parses_multi_protocol_set_with_endpoints() -> anyhow::Result<()> {
        // Case 1: one vendor serving its models over several protocols —
        // OpenAI-style under /v1 and Anthropic Messages under /anthropic.
        let src = r#"
            id = "minimax"
            display_name = "MiniMax"
            api_base = "https://api.minimax.io"

            [api_protocol]
            "*" = ["chat_completions", "responses", "messages"]

            [protocol_endpoints]
            messages = "https://api.minimax.io/anthropic/v1"

            [auth]
            kind = "bearer"
            env = "MINIMAX_API_KEY"
        "#;
        let entry: ProviderEntry = toml::from_str(src).context("required fixture value")?;
        let ProtocolMapping::PerModel(protocols) = &entry.api_protocol else {
            anyhow::bail!("expected protocol map");
        };
        assert_eq!(
            protocols.get("*").context("ordered protocols")?.as_slice(),
            &[
                ApiProtocol::ChatCompletions,
                ApiProtocol::Responses,
                ApiProtocol::Messages,
            ]
        );
        assert_eq!(
            entry.protocol_endpoints.get("messages").map(String::as_str),
            Some("https://api.minimax.io/anthropic/v1")
        );
        Ok(())
    }

    #[test]
    fn rejects_unknown_fields() -> anyhow::Result<()> {
        // Catches typos like `apt_base` → instead of silently defaulting to
        // an empty string we want a parse error at startup.
        let src = r#"
            id = "x"
            display_name = "X"
            apt_base = "https://x.example/v1"
            api_protocol = "chat_completions"
            [auth]
            kind = "bearer"
            env = "X_API_KEY"
        "#;
        assert!(toml::from_str::<ProviderEntry>(src).is_err());
        Ok(())
    }
}
