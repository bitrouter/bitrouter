//! The `server_tools.web_search` deployment config: an ordered list of search
//! backends (BYOK) the `web_search` server tool may use. Order is preference and
//! failover; the first resolvable backend is the default, and a caller's
//! per-request declaration may pin another by name. Plain data — the app
//! resolves API keys and constructs the live backends from this (see
//! `apps/bitrouter` assembly).

use serde::Deserialize;

/// Default per-call result cap when neither the caller nor the config sets one.
pub const DEFAULT_MAX_RESULTS: u32 = 5;

/// The `server_tools.web_search` section. Its presence enables the
/// `bitrouter:web_search` server tool (advertised only when a request declares
/// it). An empty `backends` list leaves the tool effectively off.
#[derive(Debug, Clone, Default, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct WebSearchSettings {
    /// Search backends in preference/failover order; the first that resolves a
    /// key is the default.
    pub backends: Vec<WebSearchBackendConfig>,
    /// Default maximum results per call (caller may lower it). Defaults to
    /// [`DEFAULT_MAX_RESULTS`].
    pub max_results: Option<u32>,
}

/// One configured search backend. The `kind` tag selects the engine; the BYOK
/// key resolves from `api_key` (supports `${VAR}` substitution) or, when
/// omitted, the engine's conventional environment variable.
#[derive(Debug, Clone, Deserialize, schemars::JsonSchema)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WebSearchBackendConfig {
    /// parallel.ai — key `api_key` or `PARALLEL_API_KEY`.
    Parallel {
        /// Explicit API key (else `PARALLEL_API_KEY`).
        #[serde(default)]
        api_key: Option<String>,
        /// Endpoint override (else the engine default).
        #[serde(default)]
        api_base: Option<String>,
    },
    /// exa.ai — key `api_key` or `EXA_API_KEY`.
    Exa {
        /// Explicit API key (else `EXA_API_KEY`).
        #[serde(default)]
        api_key: Option<String>,
        /// Endpoint override (else the engine default).
        #[serde(default)]
        api_base: Option<String>,
    },
    /// firecrawl.dev — key `api_key` or `FIRECRAWL_API_KEY`.
    Firecrawl {
        /// Explicit API key (else `FIRECRAWL_API_KEY`).
        #[serde(default)]
        api_key: Option<String>,
        /// Endpoint override (else the engine default).
        #[serde(default)]
        api_base: Option<String>,
    },
    /// tavily.com — key `api_key` or `TAVILY_API_KEY`.
    Tavily {
        /// Explicit API key (else `TAVILY_API_KEY`).
        #[serde(default)]
        api_key: Option<String>,
        /// Endpoint override (else the engine default).
        #[serde(default)]
        api_base: Option<String>,
    },
    /// A web-search-capable model whose *native* search tool is forwarded, made
    /// available to every model routed through BitRouter.
    Native {
        /// Backend id the caller pins (e.g. `"native"`); defaults to `"native"`.
        #[serde(default)]
        name: Option<String>,
        /// The search-capable model serving the call.
        model: String,
        /// The provider-defined native search tool, e.g.
        /// `{ "type": "anthropic:web_search_20250305" }`.
        tool: serde_json::Value,
    },
}
