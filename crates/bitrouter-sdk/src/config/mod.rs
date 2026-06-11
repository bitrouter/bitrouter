//! YAML configuration — gated behind the `config_file` feature.
//!
//! The [`Config`] type is the parsed shape of a `bitrouter.yaml` file. Top
//! level keys: `server`, `database`, `providers`, `models`, `presets`,
//! `variants`; per-plugin config lives under `plugins`. Load a file with
//! [`load`]; build a [`RoutingTable`](crate::language_model::RoutingTable) over
//! it with [`ConfigRoutingTable`].
//!
//! The `providers` schema is **registry-style**: `api_protocol` and
//! `rate_limits` are glob-prefix [`pattern::PatternMap`] lists, so a local
//! `bitrouter.yaml` and an external provider registry can share one schema.
//!
//! ```no_run
//! # async fn run() -> bitrouter_sdk::Result<()> {
//! use bitrouter_sdk::config::{load, ConfigRoutingTable};
//! let config = load("./bitrouter.yaml").await?;
//! let routing = ConfigRoutingTable::from_config(config);
//! # let _ = routing; Ok(()) }
//! ```

use std::collections::HashMap;

use serde::Deserialize;

use crate::error::{BitrouterError, Result};
use crate::language_model::routing::SortOrder;
use crate::language_model::types::ApiProtocol;

pub mod pattern;
pub mod presets;
pub mod routing_table;

#[cfg(test)]
mod tests;

pub use pattern::{Pattern, PatternMap};
pub use presets::{PresetResolution, PromptOverrides, resolve_presets};
pub use routing_table::ConfigRoutingTable;

/// The top-level configuration.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct Config {
    /// HTTP server settings.
    pub server: ServerConfig,
    /// Database connection settings.
    pub database: DatabaseConfig,
    /// Upstream providers, keyed by provider id.
    pub providers: HashMap<String, ProviderConfig>,
    /// Explicit virtual-model definitions (Strategy 2.2). Optional —
    /// when absent, bare model names fall through to Strategy 3 auto-cascade.
    pub models: HashMap<String, VirtualModel>,
    /// `@preset` definitions.
    pub presets: HashMap<String, PresetConfig>,
    /// `:variant` definitions.
    pub variants: HashMap<String, VariantConfig>,
    /// Plugin config, keyed by plugin / bundle id.
    pub plugins: HashMap<String, serde_json::Value>,
    /// Top-level MCP gateway settings (aggregation, caching). All fields
    /// default — `mcp:` is optional in `bitrouter.yaml`.
    pub mcp: McpConfig,
    /// Upstream MCP servers, keyed by server id. The id is what appears in
    /// `POST /mcp/{id}` and what the `mcp` pipeline's routing table looks up.
    /// Empty by default — when empty, the binary does not mount the MCP route.
    pub mcp_servers: HashMap<String, crate::mcp::transport::McpServerConfig>,
    /// Upstream ACP agents, keyed by agent id. Looked up by the `acp`
    /// pipeline's routing table; the `bitrouter agent-proxy <id>` CLI
    /// dispatches against this. Empty by default.
    pub agents: HashMap<String, crate::acp::AcpAgentConfig>,
    /// Whether providers inherit workspace defaults.
    pub inherit_defaults: bool,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            server: ServerConfig::default(),
            database: DatabaseConfig::default(),
            providers: HashMap::new(),
            models: HashMap::new(),
            presets: HashMap::new(),
            variants: HashMap::new(),
            plugins: HashMap::new(),
            mcp: McpConfig::default(),
            mcp_servers: HashMap::new(),
            agents: HashMap::new(),
            inherit_defaults: true,
        }
    }
}

/// Top-level MCP gateway settings.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Aggregation (fan-out) endpoint settings.
    pub aggregate: McpAggregateConfig,
    /// List-call cache settings.
    pub cache: McpCacheConfig,
}

/// `mcp.aggregate` — the virtual aggregate endpoint.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpAggregateConfig {
    /// When `true` (the default), mount the aggregate route. When `false`,
    /// only per-server routes (`/mcp/{server}`) are mounted.
    pub enabled: bool,
    /// HTTP path for the aggregate endpoint. Default `/mcp`.
    pub route: String,
}

impl Default for McpAggregateConfig {
    fn default() -> Self {
        Self {
            enabled: true,
            route: "/mcp".to_string(),
        }
    }
}

/// `mcp.cache` — the TTL cache wrapping cheap list calls.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct McpCacheConfig {
    /// When `false`, the cache layer is not installed.
    pub enabled: bool,
    /// TTL for `tools/list`. `0` disables for this method.
    pub tools_list_ttl_secs: u64,
    /// TTL for `resources/list`. `0` disables for this method.
    pub resources_list_ttl_secs: u64,
    /// TTL for `resources/templates/list`. `0` disables for this method.
    pub resources_templates_list_ttl_secs: u64,
    /// TTL for `prompts/list`. `0` disables for this method.
    pub prompts_list_ttl_secs: u64,
    /// LRU safety bound — max entries per server.
    pub max_entries_per_server: usize,
}

impl Default for McpCacheConfig {
    fn default() -> Self {
        // Defaults below are kept in lockstep with `mcp::CacheTtls::default()`.
        // `mcp_cache_config_defaults_match_cache_ttls` (caching_executor tests)
        // fails the build if these drift apart.
        Self {
            enabled: true,
            tools_list_ttl_secs: 60,
            resources_list_ttl_secs: 60,
            resources_templates_list_ttl_secs: 300,
            prompts_list_ttl_secs: 300,
            max_entries_per_server: 64,
        }
    }
}

/// HTTP server settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ServerConfig {
    /// `host:port` to listen on.
    pub listen: String,
    /// Unix control socket path.
    pub control_socket: String,
    /// Log level.
    pub log_level: String,
    /// SDK-level flag: when `true`, credential-less requests are admitted with
    /// a synthesised local caller. Code default is **`false`** — only the
    /// config file produced by `bitrouter init` writes `true`.
    pub skip_auth: bool,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: "0.0.0.0:4356".to_string(),
            control_socket: "./bitrouter.sock".to_string(),
            log_level: "info".to_string(),
            skip_auth: false,
        }
    }
}

/// Database connection settings.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct DatabaseConfig {
    /// Connection URL (e.g. `sqlite://./bitrouter.db`).
    pub url: String,
}

impl Default for DatabaseConfig {
    fn default() -> Self {
        Self {
            url: "sqlite://./bitrouter.db".to_string(),
        }
    }
}

/// A rate limit for one `(provider, pattern)` bucket. Limits are keyed
/// per-`(provider, matched pattern)` — two patterns with different RPMs get
/// independent windows.
#[derive(Debug, Clone, Copy, Default, Deserialize)]
pub struct RateLimit {
    /// Requests-per-minute ceiling for this bucket.
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
    /// Tokens-per-minute ceiling for this bucket.
    #[serde(default)]
    pub tokens_per_minute: Option<u32>,
}

/// Per-model pricing as written in config: micro-USD per token.
///
/// The top-level rates are the **base bracket**. [`context_tiers`] optionally
/// raises the per-token rate once a request's input (prompt) token count
/// crosses a threshold — some upstreams bill a steeper rate above a context
/// length (e.g. a higher rate past 128k input tokens). Empty ⇒ flat pricing.
///
/// [`context_tiers`]: PricingConfig::context_tiers
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PricingConfig {
    /// Micro-USD per prompt (input) token (base bracket).
    #[serde(default)]
    pub input_micro_usd_per_token: f64,
    /// Micro-USD per completion (output) token (base bracket).
    #[serde(default)]
    pub output_micro_usd_per_token: f64,
    /// Optional higher context brackets, applied by total input-token count.
    /// The selected bracket's rates apply to the whole request (a step
    /// function); the consumer's bracket pick is order-independent.
    #[serde(default)]
    pub context_tiers: Vec<PricingTierConfig>,
}

/// One higher context-pricing bracket in config — see
/// [`PricingConfig::context_tiers`].
#[derive(Debug, Clone, Default, Deserialize)]
pub struct PricingTierConfig {
    /// Exclusive lower bound on total input tokens: a request whose input
    /// size is strictly greater than this enters the bracket (a base bracket
    /// documented as "≤ 128k" is written as `above_input_tokens: 128000`).
    pub above_input_tokens: u64,
    /// Micro-USD per prompt (input) token for this bracket.
    #[serde(default)]
    pub input_micro_usd_per_token: f64,
    /// Micro-USD per completion (output) token for this bracket.
    #[serde(default)]
    pub output_micro_usd_per_token: f64,
}

/// One upstream provider entry — registry-style.
///
/// `Debug` redacts `api_key` (v0 audit S9) so a future `tracing::error!(?config, …)`
/// can't dump the platform credential straight into structured logs.
#[derive(Clone, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Upstream API base URL.
    pub api_base: String,
    /// Upstream API key (often a `${VAR}` reference).
    pub api_key: String,
    /// Glob-prefix `api_protocol` pattern list. Precedence: per-model override
    /// > longest matching pattern > provider default (`openai`).
    pub api_protocol: PatternMap<ApiProtocol>,
    /// Glob-prefix `rate_limits` pattern list, keyed per matched pattern.
    pub rate_limits: PatternMap<RateLimit>,
    /// Declared model entries. Empty + `auto_discover` triggers discovery.
    pub models: Vec<ProviderModel>,
    /// When `true` and `models` is empty, discover models from the provider's
    /// `/models` endpoint at startup and on reload.
    pub auto_discover: bool,
    /// Whether this provider is active / routable.
    pub active: bool,
    /// Free-form tags, used by `RoutingPrefs.require_tags` filtering.
    pub tags: Vec<String>,
    /// Inherit defaults from another provider in this config (acceptance F20 /
    /// v0 `derives`). The named provider's `api_protocol`, `rate_limits`,
    /// `models`, `tags` and `auto_discover` flow into *this* provider's empty
    /// fields; explicit fields here win. Resolved by
    /// [`resolve_derivations`] after the config is parsed.
    pub derives: Option<String>,
    /// Multiple credentials for this one provider — e.g. two
    /// subscriptions to the same upstream. When non-empty, the routing
    /// table expands the provider into one routing target per account;
    /// [`account_strategy`](Self::account_strategy) decides their
    /// order. Empty = a single-credential provider keyed off the
    /// top-level [`api_key`](Self::api_key) (the common case).
    pub accounts: Vec<ProviderAccount>,
    /// How the per-account targets are ordered when `accounts` is set.
    pub account_strategy: AccountStrategy,
}

/// One credential within a multi-account provider. An account varies
/// only the credential (and optionally the base URL) — the protocol,
/// model catalog, and rate limits are the provider's.
#[derive(Clone, Default, Deserialize)]
#[serde(default)]
pub struct ProviderAccount {
    /// This account's API key (often a `${VAR}` reference).
    pub api_key: String,
    /// Optional per-account `api_base` override. Empty inherits the
    /// provider's `api_base` — set it only for multi-region / multi-org
    /// deployments where each account lives on a different host.
    pub api_base: String,
    /// Optional human label, surfaced in the request log so an operator
    /// can see which account served a request. Defaults to
    /// `account-<n>` (1-based) when empty.
    pub label: String,
}

impl std::fmt::Debug for ProviderAccount {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Redact the key for the same reason `ProviderConfig` does.
        f.debug_struct("ProviderAccount")
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("api_base", &self.api_base)
            .field("label", &self.label)
            .finish()
    }
}

/// How a multi-account provider's per-account targets are ordered.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AccountStrategy {
    /// Accounts in declared order — the first is primary; routing drops
    /// to the next on a retryable failure (5xx / 429 / timeout) or a
    /// payment / credit-exhaustion error. The default.
    #[default]
    Failover,
    /// Accounts ordered with a process-random rotation so the primary
    /// (and therefore the load) spreads evenly across accounts. The
    /// remaining accounts still act as failover targets for that
    /// request.
    Balance,
}

impl std::fmt::Debug for ProviderConfig {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("ProviderConfig")
            .field("api_base", &self.api_base)
            .field(
                "api_key",
                &if self.api_key.is_empty() {
                    "<empty>"
                } else {
                    "<redacted>"
                },
            )
            .field("api_protocol", &self.api_protocol)
            .field("rate_limits", &self.rate_limits)
            .field("models", &self.models)
            .field("auto_discover", &self.auto_discover)
            .field("active", &self.active)
            .field("tags", &self.tags)
            .field("derives", &self.derives)
            .field("accounts", &self.accounts)
            .field("account_strategy", &self.account_strategy)
            .finish()
    }
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_base: String::new(),
            api_key: String::new(),
            api_protocol: PatternMap::new(),
            rate_limits: PatternMap::new(),
            models: Vec::new(),
            auto_discover: false,
            active: true,
            tags: Vec::new(),
            derives: None,
            accounts: Vec::new(),
            account_strategy: AccountStrategy::default(),
        }
    }
}

impl ProviderConfig {
    /// The API key for provider-level operations that aren't tied to a
    /// specific routed request — currently model discovery's `/models`
    /// probe. Returns the top-level [`api_key`](Self::api_key), or the
    /// first account's key when the provider is account-based and has
    /// no top-level key.
    pub fn primary_api_key(&self) -> &str {
        if !self.api_key.is_empty() {
            return &self.api_key;
        }
        self.accounts
            .iter()
            .map(|a| a.api_key.as_str())
            .find(|k| !k.is_empty())
            .unwrap_or("")
    }
}

impl ProviderConfig {
    /// Resolve the effective `ApiProtocol` for `model_id`: per-model override
    /// wins, then the longest matching `api_protocol` pattern, then the
    /// provider default (`openai`). Includes the `auto_discover` protocol
    /// inference from the api-base host.
    pub fn protocol_for(&self, model_id: &str) -> ApiProtocol {
        if let Some(m) = self.models.iter().find(|m| m.id == model_id)
            && let Some(p) = &m.api_protocol
        {
            return p.clone();
        }
        if let Some(p) = self.api_protocol.resolve(model_id) {
            return p.clone();
        }
        infer_protocol(&self.api_base)
    }
}

/// Infer a wire protocol from a provider's api-base host.
pub fn infer_protocol(api_base: &str) -> ApiProtocol {
    let host = api_base.to_ascii_lowercase();
    if host.contains("anthropic.com") {
        ApiProtocol::Messages
    } else if host.contains("googleapis.com") || host.contains("generativelanguage") {
        ApiProtocol::GenerateContent
    } else {
        ApiProtocol::ChatCompletions
    }
}

/// One model entry under a provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderModel {
    /// Model id at the provider.
    pub id: String,
    /// Per-model protocol override (highest precedence).
    #[serde(default)]
    pub api_protocol: Option<ApiProtocol>,
    /// Per-model rate-limit override.
    #[serde(default)]
    pub rate_limits: Option<RateLimit>,
    /// Per-model pricing.
    #[serde(default)]
    pub pricing: Option<PricingConfig>,
}

/// An explicit virtual-model definition (Strategy 2).
#[derive(Debug, Clone, Deserialize)]
pub struct VirtualModel {
    /// How this virtual model's [`endpoints`](Self::endpoints) are ordered
    /// into the fallback chain. See [`VirtualModelStrategy`]. Defaults to
    /// [`Priority`](VirtualModelStrategy::Priority).
    #[serde(default)]
    pub strategy: VirtualModelStrategy,
    /// The endpoints this virtual model maps to. Their meaning depends on
    /// [`strategy`](Self::strategy): under `priority` they are an *ordered*
    /// preference list; under `cascade` the order is a starting point that
    /// the cascade ordering then re-sorts.
    pub endpoints: Vec<VirtualEndpoint>,
    /// Optional pricing for the virtual model.
    #[serde(default)]
    pub pricing: Option<PricingConfig>,
}

/// How a [`VirtualModel`]'s endpoints are ordered into its fallback chain
/// (Strategy 2). Mirrors the typed-enum + serde pattern of
/// [`AccountStrategy`].
///
/// Both strategies build a chain that `execute_with_fallback` walks: a
/// retryable failure (5xx / 408 / 429 / timeout / credit-exhaustion) always
/// advances to the next endpoint — that failover-on-error behaviour is a
/// property of *any* chain and is shared by both. They differ only in the
/// **order** the endpoints take in that chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum VirtualModelStrategy {
    /// Endpoints are tried in **declared YAML order** — the first is the
    /// preferred endpoint, the rest are failover targets reached only when a
    /// preceding one fails with a retryable error. Routing preferences
    /// (`sort` / `only` / `ignore`) do **not** reorder or filter the chain:
    /// the operator's declared priority is authoritative. This is the
    /// default and matches the historical (pre-typed-`strategy`) behaviour.
    #[default]
    Priority,
    /// Endpoints are treated as an unordered candidate set and **re-ordered by
    /// the request's [`SortOrder`]** (the same cascade ordering Strategy-3
    /// auto-cascade applies to providers), and filtered by `only` / `ignore` /
    /// `require_tags`.
    /// Use this when the endpoints are interchangeable and you want
    /// cost/latency-aware (or, today, alphabetical) selection rather than a
    /// fixed priority order. `Latency` / `Cost` have no metrics source yet, so
    /// they currently fall back to alphabetical-by-provider — the same honest
    /// limitation Strategy-3 documents.
    Cascade,
}

/// One endpoint of a virtual model.
#[derive(Debug, Clone, Deserialize)]
pub struct VirtualEndpoint {
    /// The provider id this endpoint routes to.
    pub provider: String,
    /// The service / model id at that provider.
    pub service_id: String,
}

/// Routing knobs shared by presets and variants.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct RoutingConfig {
    /// Cascade-chain ordering.
    pub sort: Option<SortOrder>,
    /// Require providers carrying all these tags.
    pub require_tags: Vec<String>,
    /// Restrict the chain to exactly these providers.
    pub only: Vec<String>,
    /// Drop these providers from the chain.
    pub ignore: Vec<String>,
}

/// An `@preset` definition.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct PresetConfig {
    /// Substitute the request's model with this one.
    pub model: Option<String>,
    /// Prepend / replace the system prompt.
    pub system_prompt: Option<String>,
    /// Generation-parameter overrides, merged shallowly into the request.
    pub params: serde_json::Map<String, serde_json::Value>,
    /// Routing knobs fed into the cascade.
    pub routing: RoutingConfig,
}

/// A `:variant` definition — a routing modifier only.
#[derive(Debug, Clone, Default, Deserialize)]
#[serde(default)]
pub struct VariantConfig {
    /// Routing knobs fed into the cascade.
    pub routing: RoutingConfig,
}

// ===== `${VAR}` substitution + loader =====

/// Replace every `${VAR}` occurrence with the value resolved by `lookup`. An
/// unresolved variable is an error (config F8: `${VAR}` substitution).
pub fn substitute_with<F>(input: &str, lookup: F) -> Result<String>
where
    F: Fn(&str) -> Option<String>,
{
    let mut out = String::with_capacity(input.len());
    let mut rest = input;
    while let Some(start) = rest.find("${") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        match after.find('}') {
            Some(end) => {
                let name = &after[..end];
                let value = lookup(name).ok_or_else(|| {
                    BitrouterError::bad_request(format!(
                        "config references undefined environment variable '{name}'"
                    ))
                })?;
                out.push_str(&value);
                rest = &after[end + 1..];
            }
            None => {
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Replace every `${VAR}` occurrence with the value of environment variable
/// `VAR`. An undefined variable is an error. Used by the config loader.
/// Reads via [`env_lookup`] so daemon-side overrides (installed by the
/// CLI's `bitrouter reload`) take precedence over the live process env.
pub fn substitute_env(input: &str) -> Result<String> {
    substitute_with(input, env_lookup)
}

/// Process-global env-var override map. Read by [`env_lookup`] before
/// falling back to [`std::env::var`]. Written by [`set_env_overrides`]
/// — the daemon calls that when the CLI sends a `Reload { env }`
/// command, so a freshly-`export`ed API key in the user's shell
/// propagates into the running daemon without restarting it.
///
/// `RwLock` (not unsafe `set_var`) keeps things `#![forbid(unsafe_code)]`
/// clean and avoids the cross-thread soundness footgun of mutating the
/// real process env while other threads might be reading it.
static ENV_OVERRIDES: std::sync::OnceLock<
    std::sync::RwLock<std::collections::HashMap<String, String>>,
> = std::sync::OnceLock::new();

fn overrides() -> &'static std::sync::RwLock<std::collections::HashMap<String, String>> {
    ENV_OVERRIDES.get_or_init(|| std::sync::RwLock::new(std::collections::HashMap::new()))
}

/// Replace the in-memory override map atomically. Subsequent
/// [`env_lookup`] / [`substitute_env`] calls — and
/// `bitrouter_providers::zero_config`, which resolves through
/// `env_lookup` — see the new values. Empty map clears all overrides.
pub fn set_env_overrides(values: std::collections::HashMap<String, String>) {
    let mut w = overrides().write().expect("env override lock poisoned");
    *w = values;
}

/// Resolve an env var name to a value. Checks the in-memory override
/// map first; falls back to [`std::env::var`]. Returns `None` for an
/// unknown name. This is the function every config-loading path goes
/// through — `${VAR}` substitution in YAML, `zero_config`'s
/// "is this provider's key set" check, etc.
pub fn env_lookup(name: &str) -> Option<String> {
    if let Some(rw) = ENV_OVERRIDES.get()
        && let Ok(guard) = rw.read()
        && let Some(value) = guard.get(name)
    {
        return Some(value.clone());
    }
    std::env::var(name).ok()
}

/// Parse a config from a YAML string, applying `${VAR}` substitution first.
pub fn parse(yaml: &str) -> Result<Config> {
    parse_with(yaml, |name| std::env::var(name).ok())
}

/// Parse a config from a YAML string, resolving `${VAR}` via `lookup`. Useful
/// for tests that must not mutate process-global environment state.
pub fn parse_with<F>(yaml: &str, lookup: F) -> Result<Config>
where
    F: Fn(&str) -> Option<String>,
{
    let substituted = substitute_with(yaml, lookup)?;
    let mut config: Config = serde_saphyr::from_str(&substituted)
        .map_err(|e| BitrouterError::bad_request(format!("invalid bitrouter.yaml: {e}")))?;
    resolve_derivations(&mut config)?;
    // SSRF defence (v0 audit S4): refuse a config that asks bitrouter to
    // route at a loopback / private / metadata URL. A typo or a malicious
    // YAML otherwise has the executor send every upstream request — and
    // any API keys / prompt bodies — into the host's internal network.
    // Validated post-`resolve_derivations` so an inherited `api_base` is
    // checked against the *effective* value.
    for (id, provider) in &config.providers {
        if !provider.api_base.is_empty() {
            crate::url_validator::validate_upstream_url(&provider.api_base).map_err(|e| {
                BitrouterError::bad_request(format!("provider '{id}' api_base rejected: {e}"))
            })?;
        }
        // A per-account `api_base` override reaches the executor exactly like
        // the provider-level one (`build_targets` copies it onto the routing
        // target), so it needs the same SSRF gate — otherwise an `accounts`
        // entry is an unchecked back door to the host's internal network. An
        // empty override inherits the (already-validated) provider `api_base`.
        for account in &provider.accounts {
            if !account.api_base.is_empty() {
                crate::url_validator::validate_upstream_url(&account.api_base).map_err(|e| {
                    let who = if account.label.is_empty() {
                        format!("provider '{id}' account")
                    } else {
                        format!("provider '{id}' account '{}'", account.label)
                    };
                    BitrouterError::bad_request(format!("{who} api_base rejected: {e}"))
                })?;
            }
        }
    }
    Ok(config)
}

/// Resolve every provider's `derives` chain: any field this provider left
/// empty / default flows from the named ancestor. Cycles are a 400.
/// Resolution walks the chain depth-first; multi-level inheritance works.
pub fn resolve_derivations(config: &mut Config) -> Result<()> {
    let ids: Vec<String> = config.providers.keys().cloned().collect();
    for id in ids {
        let mut seen: Vec<String> = vec![id.clone()];
        resolve_one_derivation(&mut config.providers, &id, &mut seen)?;
    }
    Ok(())
}

fn resolve_one_derivation(
    providers: &mut HashMap<String, ProviderConfig>,
    id: &str,
    seen: &mut Vec<String>,
) -> Result<()> {
    let derives_target = providers.get(id).and_then(|p| p.derives.clone());
    let Some(parent_id) = derives_target else {
        return Ok(());
    };
    if seen.contains(&parent_id) {
        return Err(BitrouterError::bad_request(format!(
            "provider '{id}' derives chain has a cycle through '{parent_id}'"
        )));
    }
    if !providers.contains_key(&parent_id) {
        return Err(BitrouterError::bad_request(format!(
            "provider '{id}' derives from unknown provider '{parent_id}'"
        )));
    }
    seen.push(parent_id.clone());
    resolve_one_derivation(providers, &parent_id, seen)?;
    let parent = providers.get(&parent_id).cloned().expect("checked above");
    let child = providers.get_mut(id).expect("we own this entry");
    // Inherit each empty field from the parent. Explicit non-empty fields on
    // the child win. `api_base` / `api_key` are NOT inherited — they are
    // intrinsic to the child (you almost always want different endpoints).
    if child.api_protocol.is_empty() {
        child.api_protocol = parent.api_protocol.clone();
    }
    if child.rate_limits.is_empty() {
        child.rate_limits = parent.rate_limits.clone();
    }
    if child.models.is_empty() {
        child.models = parent.models.clone();
    }
    if !parent.tags.is_empty() && child.tags.is_empty() {
        child.tags = parent.tags.clone();
    }
    // auto_discover propagates only when the child didn't explicitly set it;
    // since it's a bool with default false, we propagate when child is false
    // AND parent is true.
    if !child.auto_discover && parent.auto_discover {
        child.auto_discover = true;
    }
    // The `derives` link itself doesn't carry into the resolved form; clearing
    // it makes repeated calls idempotent.
    child.derives = None;
    Ok(())
}

/// Load and parse `bitrouter.yaml` from disk.
pub async fn load(path: impl AsRef<std::path::Path>) -> Result<Config> {
    let path = path.as_ref();
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| BitrouterError::internal(format!("reading config {}: {e}", path.display())))?;
    parse(&raw)
}

/// Best-effort model discovery. For every provider with
/// `auto_discover: true` and no declared `models`, `GET {api_base}/models` and
/// populate the model list from the response (`{ "data": [{ "id": … }] }`,
/// the Chat Completions / Messages shape). A provider whose discovery call fails is
/// left as-is with a WARN — discovery never aborts startup.
///
/// The HTTP client is built with bounded `connect_timeout` + `timeout` so an
/// unreachable provider can't stall a `bitrouter reload` for the OS-level
/// connect window (minutes). Discovery is best-effort; a 5s overall cap is
/// well above any healthy `/models` round-trip and far below the default.
pub async fn discover_models(config: &mut Config) {
    let client = reqwest::Client::builder()
        .connect_timeout(std::time::Duration::from_secs(2))
        .timeout(std::time::Duration::from_secs(5))
        .build()
        .unwrap_or_else(|_| reqwest::Client::new());
    for (provider_id, provider) in config.providers.iter_mut() {
        if !provider.auto_discover || !provider.models.is_empty() {
            continue;
        }
        let url = format!("{}/models", provider.api_base.trim_end_matches('/'));
        let result = async {
            let resp = client
                .get(&url)
                .bearer_auth(provider.primary_api_key())
                .send()
                .await
                .ok()?;
            if !resp.status().is_success() {
                return None;
            }
            let json: serde_json::Value = resp.json().await.ok()?;
            let ids: Vec<String> = json
                .get("data")?
                .as_array()?
                .iter()
                .filter_map(|m| m.get("id").and_then(|i| i.as_str()).map(String::from))
                .collect();
            Some(ids)
        }
        .await;
        match result {
            Some(ids) if !ids.is_empty() => {
                tracing::info!(provider = %provider_id, count = ids.len(), "auto-discovered models");
                provider.models = ids
                    .into_iter()
                    .map(|id| ProviderModel {
                        id,
                        api_protocol: None,
                        rate_limits: None,
                        pricing: None,
                    })
                    .collect();
            }
            _ => {
                tracing::warn!(
                    provider = %provider_id,
                    %url,
                    "auto_discover: model discovery failed — provider left with no models"
                );
            }
        }
    }
}
