//! The `server_tools.web_fetch` deployment config: an ordered list of BYOK
//! extraction backends the `web_fetch` server tool may use. Order is preference
//! and failover; the first resolvable backend is the default, and a caller's
//! per-request declaration may pin another by name. Plain data — the app
//! resolves API keys and constructs the live backends from this (see
//! `apps/bitrouter` assembly).

use serde::Deserialize;

/// Default per-call content cap (in tokens) when neither the caller nor the
/// config sets one.
pub const DEFAULT_MAX_CONTENT_TOKENS: u32 = 25_000;

/// The `server_tools.web_fetch` section. Its presence enables the
/// `bitrouter:web_fetch` server tool (advertised only when a request declares
/// it). An empty `backends` list leaves the tool effectively off.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WebFetchSettings {
    /// Extraction backends in preference/failover order; the first that resolves
    /// a key is the default.
    pub backends: Vec<WebFetchBackendConfig>,
    /// Default cap on returned content per fetch, in tokens (caller may lower
    /// it). Defaults to [`DEFAULT_MAX_CONTENT_TOKENS`].
    pub max_content_tokens: Option<u32>,
}

/// One configured extraction backend. The `kind` tag selects the engine; the
/// BYOK key resolves from `api_key` (supports `${VAR}` substitution) or, when
/// omitted, the engine's conventional environment variable.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebFetchBackendConfig {
    /// exa.ai `/contents` — key `api_key` or `EXA_API_KEY`.
    Exa {
        /// Explicit API key (else `EXA_API_KEY`).
        #[serde(default)]
        api_key: Option<String>,
        /// Endpoint override (else the engine default).
        #[serde(default)]
        api_base: Option<String>,
    },
    /// firecrawl.dev `/v2/scrape` — key `api_key` or `FIRECRAWL_API_KEY`.
    Firecrawl {
        /// Explicit API key (else `FIRECRAWL_API_KEY`).
        #[serde(default)]
        api_key: Option<String>,
        /// Endpoint override (else the engine default).
        #[serde(default)]
        api_base: Option<String>,
    },
    /// tavily.com `/extract` — key `api_key` or `TAVILY_API_KEY`.
    Tavily {
        /// Explicit API key (else `TAVILY_API_KEY`).
        #[serde(default)]
        api_key: Option<String>,
        /// Endpoint override (else the engine default).
        #[serde(default)]
        api_base: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn deserializes_backends_in_order() {
        let s: WebFetchSettings = serde_json::from_value(json!({
            "backends": [
                { "kind": "exa", "api_key": "k" },
                { "kind": "firecrawl" },
                { "kind": "tavily", "api_base": "https://example/extract" }
            ],
            "max_content_tokens": 1000
        }))
        .unwrap();
        assert_eq!(s.backends.len(), 3);
        assert_eq!(s.max_content_tokens, Some(1000));
        assert!(matches!(s.backends[0], WebFetchBackendConfig::Exa { .. }));
        assert!(matches!(
            s.backends[1],
            WebFetchBackendConfig::Firecrawl { .. }
        ));
        assert!(matches!(
            s.backends[2],
            WebFetchBackendConfig::Tavily { .. }
        ));
    }

    #[test]
    fn defaults_to_empty() {
        let s = WebFetchSettings::default();
        assert!(s.backends.is_empty());
        assert!(s.max_content_tokens.is_none());
    }
}
