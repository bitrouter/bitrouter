use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use bitrouter_core::routers::routing_table::ApiProtocol;

use crate::env::{load_env, substitute_in_value};
use crate::registry::{
    builtin_agent_defs, builtin_providers, builtin_tool_provider_defs, merge_provider,
    resolve_providers,
};

fn default_true() -> bool {
    true
}

// ── Policy configuration ────────────────────────────────────────────

// ── Top-level configuration ──────────────────────────────────────────

/// Root configuration file, typically `bitrouter.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BitrouterConfig {
    #[serde(default)]
    pub server: ServerConfig,

    /// Database configuration.
    #[serde(default)]
    pub database: DatabaseConfig,

    /// Guardrails configuration — content inspection firewall for AI traffic.
    #[serde(default)]
    pub guardrails: bitrouter_guardrails::GuardrailConfig,

    /// Solana RPC endpoint used for Swig wallet operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_rpc_url: Option<String>,

    /// MPP (Machine Payment Protocol) configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mpp: Option<MppConfig>,

    /// OWS (Open Wallet Standard) wallet configuration.
    ///
    /// When set, the OWS wallet is used for policy-gated signing in place
    /// of raw private keys. Requires the `wallet-ows` feature.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wallet: Option<WalletConfig>,

    /// When `true` (the default), built-in provider definitions are merged
    /// into the provider set before user overrides are applied.  Set to
    /// `false` to use *only* the providers declared in the config file.
    #[serde(default = "default_true")]
    pub inherit_defaults: bool,

    /// Provider definitions (merged on top of built-in providers).
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,

    /// Model routing definitions.
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,

    /// Tool routing definitions.
    #[serde(default)]
    pub tools: HashMap<String, ToolConfig>,

    /// Agent definitions (ACP-compatible coding agents).
    #[serde(default)]
    pub agents: HashMap<String, AgentConfig>,

    /// Content-based auto-routing rules.
    ///
    /// Each key is a virtual model name (e.g. `"auto"`) that triggers
    /// content-aware classification when a request targets it. The rule
    /// maps detected signals and complexity levels to concrete model names
    /// defined in the `models` section.
    #[serde(default)]
    pub routing: HashMap<String, RoutingRuleConfig>,
}

impl BitrouterConfig {
    /// Returns true if at least one provider has an API key configured.
    pub fn has_configured_providers(&self) -> bool {
        self.providers.values().any(|p| p.api_key.is_some())
    }

    /// Returns the names of providers that have API keys configured.
    pub fn configured_provider_names(&self) -> Vec<String> {
        let mut names: Vec<String> = self
            .providers
            .iter()
            .filter(|(_, p)| p.api_key.is_some())
            .map(|(name, _)| name.clone())
            .collect();
        names.sort();
        names
    }

    /// Full config loading pipeline:
    ///
    /// 1. Read and parse YAML
    /// 2. Load `.env` file (provided externally by the runtime)
    /// 3. Substitute `${VAR}` references in all string values
    /// 4. Merge user providers on top of the built-in registry
    /// 5. Resolve `derives` chains
    /// 6. Apply `env_prefix` auto-overrides
    pub fn load_from_file(path: &Path, env_file: Option<&Path>) -> crate::error::Result<Self> {
        let raw =
            std::fs::read_to_string(path).map_err(|e| crate::error::ConfigError::ConfigRead {
                path: path.to_path_buf(),
                source: e,
            })?;
        Self::load_from_str(&raw, env_file)
    }

    /// Loads from an in-memory YAML string (useful for testing).
    ///
    /// The optional `env_file` path is resolved by the caller (runtime layer).
    pub fn load_from_str(raw: &str, env_file: Option<&Path>) -> crate::error::Result<Self> {
        // Load environment (.env + process env)
        let env = load_env(env_file);

        // Substitute env vars in the YAML tree, then deserialize.
        // A comments-only YAML file parses as `null`; treat it as an empty object
        // so that all `#[serde(default)]` fields are populated normally.
        let yaml_value: serde_json::Value = serde_saphyr::from_str(raw)
            .map_err(|e| crate::error::ConfigError::ConfigParse(e.to_string()))?;
        let substituted = substitute_in_value(yaml_value, &env);
        let substituted = if substituted.is_null() {
            serde_json::Value::Object(serde_json::Map::new())
        } else {
            substituted
        };
        let mut config: BitrouterConfig = serde_json::from_value(substituted)
            .map_err(|e| crate::error::ConfigError::ConfigParse(e.to_string()))?;

        // Merge built-in providers with user overrides (unless opted out)
        let mut providers = if config.inherit_defaults {
            let mut base = builtin_providers();
            for (name, user_provider) in config.providers.drain() {
                if let Some(existing) = base.get_mut(&name) {
                    merge_provider(existing, user_provider);
                } else {
                    base.insert(name, user_provider);
                }
            }
            base
        } else {
            std::mem::take(&mut config.providers)
        };

        // Merge built-in tool provider definitions (providers + tool routes).
        // Uses the same merge_provider pattern as model providers so that
        // a user declaring `exa: api_key: "..."` inherits the builtin
        // api_protocol, api_base, auth, etc.
        if config.inherit_defaults {
            for (name, builtin) in builtin_tool_provider_defs() {
                if let Some(existing) = providers.get_mut(&name) {
                    // User declared this provider — merge builtin as base.
                    let mut base = builtin.config;
                    merge_provider(&mut base, std::mem::take(existing));
                    *existing = base;
                } else {
                    providers.insert(name, builtin.config);
                }
                for (tool_name, tool_config) in builtin.tool_configs {
                    config.tools.entry(tool_name).or_insert(tool_config);
                }
            }
        }

        // Merge built-in agent definitions.
        // User-declared agents override built-ins by name.
        if config.inherit_defaults {
            for (name, builtin) in builtin_agent_defs() {
                config.agents.entry(name).or_insert(builtin);
            }
        }

        // Resolve derives + env_prefix
        config.providers = resolve_providers(providers, &env);

        Ok(config)
    }
}

// ── Agent configuration ──────────────────────────────────────────────

/// Communication protocol for an agent.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum AgentProtocol {
    /// Agent Client Protocol (JSON-RPC over stdio).
    #[default]
    Acp,
}

impl fmt::Display for AgentProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Acp => write!(f, "acp"),
        }
    }
}

/// A downloadable binary archive for a specific platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BinaryArchive {
    /// URL to a `.tar.gz` or `.zip` archive.
    pub archive: String,
    /// Command to run within the extracted archive (relative path).
    pub cmd: String,
    /// Additional arguments passed when launching the binary.
    #[serde(default)]
    pub args: Vec<String>,
}

/// How to obtain an agent if its binary is not on PATH.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Distribution {
    /// Run via `npx <package> [args...]`.
    Npx {
        package: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Run via `uvx <package> [args...]`.
    Uvx {
        package: String,
        #[serde(default)]
        args: Vec<String>,
    },
    /// Download a platform-specific binary archive.
    Binary {
        /// Map of platform target (e.g. `darwin-aarch64`) to archive info.
        platforms: HashMap<String, BinaryArchive>,
    },
}

/// Session pool configuration for an agent.
///
/// Controls how many concurrent sessions can run and when idle sessions
/// are cleaned up. When omitted from agent config, defaults produce
/// single-session behavior (compatible with the TUI path).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentSessionConfig {
    /// Idle timeout in seconds before a session is cleaned up.
    /// Default: 600 (10 minutes).
    #[serde(default = "default_idle_timeout_secs")]
    pub idle_timeout_secs: u64,

    /// Maximum number of concurrent sessions for this agent.
    /// Default: 1.
    #[serde(default = "default_max_concurrent")]
    pub max_concurrent: usize,
}

fn default_idle_timeout_secs() -> u64 {
    600
}

fn default_max_concurrent() -> usize {
    1
}

impl Default for AgentSessionConfig {
    fn default() -> Self {
        Self {
            idle_timeout_secs: default_idle_timeout_secs(),
            max_concurrent: default_max_concurrent(),
        }
    }
}

/// A2A exposure configuration for an agent.
///
/// Controls whether the agent is exposed via the A2A protocol.
/// Consumed by downstream endpoint wiring, not by this crate.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct AgentA2aConfig {
    /// Whether to expose this agent via A2A.
    #[serde(default)]
    pub enabled: bool,

    /// Skills advertised in the A2A Agent Card.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skills: Vec<String>,
}

/// Configuration for a single agent.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AgentConfig {
    /// Communication protocol.
    #[serde(default)]
    pub protocol: AgentProtocol,

    /// Binary name or path. Resolved from PATH if relative.
    pub binary: String,

    /// Arguments passed when spawning the agent subprocess.
    #[serde(default)]
    pub args: Vec<String>,

    /// Whether this agent is enabled (available for connection).
    #[serde(default = "default_true")]
    pub enabled: bool,

    /// Ordered list of distribution methods (tried in sequence as fallbacks).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub distribution: Vec<Distribution>,

    /// Session pool configuration (idle timeout, concurrency cap).
    ///
    /// When omitted, defaults to single-session behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session: Option<AgentSessionConfig>,

    /// A2A exposure configuration.
    ///
    /// When omitted or `enabled: false`, the agent is not exposed via A2A.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a: Option<AgentA2aConfig>,
}

// ── Database configuration ────────────────────────────────────────────

/// Database connection configuration.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct DatabaseConfig {
    /// Database connection URL.
    ///
    /// Supports `sqlite://`, `postgres://`, and `mysql://` schemes.
    /// Accepts `${VAR}` environment variable placeholders.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
}

// ── Server configuration ─────────────────────────────────────────────

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    #[serde(default = "default_listen")]
    pub listen: SocketAddr,

    #[serde(default)]
    pub control: ControlEndpoint,

    #[serde(default = "default_log_level")]
    pub log_level: String,
}

impl Default for ServerConfig {
    fn default() -> Self {
        Self {
            listen: default_listen(),
            control: ControlEndpoint::default(),
            log_level: default_log_level(),
        }
    }
}

fn default_listen() -> SocketAddr {
    SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), 8787)
}

fn default_log_level() -> String {
    "info".into()
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ControlEndpoint {
    #[serde(default = "default_socket_path")]
    pub socket: PathBuf,
}

impl Default for ControlEndpoint {
    fn default() -> Self {
        Self {
            socket: default_socket_path(),
        }
    }
}

fn default_socket_path() -> PathBuf {
    PathBuf::from("bitrouter.sock")
}

// ── Provider configuration ───────────────────────────────────────────

/// Configuration for a single provider.
///
/// All fields are `Option` so that partial overlays via `derives` work correctly:
/// only the fields the user explicitly sets will override the parent.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ProviderConfig {
    /// Inherit from another provider.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub derives: Option<String>,

    /// The API protocol / adapter to use.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_protocol: Option<ApiProtocol>,

    /// Base URL for the upstream API.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,

    /// Default API key.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// Auth configuration override (e.g. custom auth methods).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auth: Option<AuthConfig>,

    /// Environment variable prefix for auto-loading
    /// `{PREFIX}_API_KEY` / `{PREFIX}_BASE_URL`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_prefix: Option<String>,

    /// Extra default HTTP headers sent with every request.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_headers: Option<HashMap<String, String>>,

    /// Per-model metadata and pricing catalog.
    ///
    /// Keys are upstream model IDs (e.g. `"gpt-4o"`). Values carry optional
    /// display name, description, context length, supported modalities, and
    /// token pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub models: Option<HashMap<String, ModelInfo>>,

    // ── MCP-specific provider fields ────────────────────────────────
    /// When `true`, this MCP provider is also exposed as a standalone
    /// Streamable HTTP endpoint at `POST /mcp/{name}` and `GET /mcp/{name}/sse`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bridge: Option<bool>,
}

// ── Model metadata & pricing ─────────────────────────────────────────

/// Media modality supported by a model.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Modality {
    Text,
    Image,
    Audio,
    Video,
    File,
}

impl fmt::Display for Modality {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Text => "text",
            Self::Image => "image",
            Self::Audio => "audio",
            Self::Video => "video",
            Self::File => "file",
        })
    }
}

/// Metadata and pricing for a single model offered by a provider.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelInfo {
    /// Human-readable display name (e.g. "GPT-4o").
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Brief description of the model's capabilities.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// Maximum input context window in tokens.
    ///
    /// Accepts both `max_input_tokens` and the legacy `context_length` name in
    /// YAML; they map to the same field.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        alias = "context_length"
    )]
    pub max_input_tokens: Option<u64>,

    /// Maximum number of output tokens the model can produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,

    /// Input modalities the model accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<Modality>,

    /// Output modalities the model can produce.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<Modality>,

    /// Token pricing per million tokens.
    #[serde(default)]
    pub pricing: ModelPricing,
}

// Model pricing types are defined in `bitrouter-core::routers::routing_table`
// and re-exported from this crate's `lib.rs` for backward compatibility.
pub use bitrouter_core::routers::routing_table::{
    InputTokenPricing, ModelPricing, OutputTokenPricing,
};

// ── MPP (Machine Payment Protocol) configuration ─────────────────────

/// Top-level MPP configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MppConfig {
    /// Whether MPP payment gating is enabled.
    #[serde(default)]
    pub enabled: bool,

    /// Server realm for `WWW-Authenticate` headers.
    ///
    /// Auto-detected from environment if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub realm: Option<String>,

    /// HMAC secret for stateless challenge ID verification.
    ///
    /// Reads `MPP_SECRET_KEY` environment variable if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub secret_key: Option<String>,

    /// Per-network configuration.
    ///
    /// Each supported payment network (Tempo, Solana, …) has its own
    /// section with a network-specific recipient address and settings.
    #[serde(default)]
    pub networks: MppNetworksConfig,
}

/// Per-network MPP configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct MppNetworksConfig {
    /// Tempo network configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tempo: Option<TempoMppConfig>,

    /// Solana network configuration.
    #[cfg(feature = "payments-solana")]
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana: Option<SolanaMppConfig>,
}

/// Tempo-specific MPP configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TempoMppConfig {
    /// Recipient address for payments (required).
    pub recipient: String,

    /// Escrow contract address (required for session support).
    pub escrow_contract: String,

    /// Tempo RPC endpoint URL.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rpc_url: Option<String>,

    /// TIP-20 token address for charges.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub currency: Option<String>,

    /// Enable fee sponsorship for all challenges.
    #[serde(default)]
    pub fee_payer: bool,

    /// EVM hex private key for server-initiated channel close and settlement.
    /// When set, the server can call `close()` on the escrow contract on behalf
    /// of the payee after a client sends a close credential.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub close_signer: Option<String>,

    /// Default deposit amount (in base units) for client-side session channels.
    /// Used when the server challenge does not include `suggestedDeposit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_deposit: Option<String>,
}

/// Solana-specific MPP configuration.
#[cfg(feature = "payments-solana")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolanaMppConfig {
    /// Recipient address for payments (Solana base58 pubkey, required).
    pub recipient: String,

    /// Channel (escrow) program address (required for session support).
    pub channel_program: String,

    /// Solana network name (e.g., "mainnet-beta", "devnet").
    #[serde(default = "default_solana_network")]
    pub network: String,

    /// Payment asset configuration. Defaults to native SOL.
    #[serde(default)]
    pub asset: SolanaAssetConfig,

    /// Default deposit amount (in base units) suggested to clients when
    /// opening a session channel. Included in the 402 challenge as
    /// `sessionDefaults.suggestedDeposit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub suggested_deposit: Option<String>,
}

/// Payment asset descriptor for Solana MPP.
#[cfg(feature = "payments-solana")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolanaAssetConfig {
    /// Asset kind: `"sol"` for native SOL, `"spl"` for an SPL token.
    #[serde(default = "default_solana_asset_kind")]
    pub kind: String,

    /// Decimal precision (9 for SOL, 6 for USDC).
    #[serde(default = "default_solana_asset_decimals")]
    pub decimals: u8,

    /// SPL token mint address. Required when `kind` is `"spl"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mint: Option<String>,

    /// Display symbol (e.g. `"SOL"`, `"USDC"`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub symbol: Option<String>,
}

#[cfg(feature = "payments-solana")]
impl Default for SolanaAssetConfig {
    fn default() -> Self {
        Self {
            kind: default_solana_asset_kind(),
            decimals: default_solana_asset_decimals(),
            mint: None,
            symbol: None,
        }
    }
}

#[cfg(feature = "payments-solana")]
fn default_solana_asset_kind() -> String {
    "sol".into()
}

#[cfg(feature = "payments-solana")]
fn default_solana_asset_decimals() -> u8 {
    9
}

#[cfg(feature = "payments-solana")]
fn default_solana_network() -> String {
    "mainnet-beta".into()
}

// ── Wallet configuration ─────────────────────────────────────────────

/// OWS (Open Wallet Standard) wallet configuration.
///
/// When present, BitRouter uses the named OWS wallet for signing
/// operations (e.g. MPP close transactions) instead of raw private keys.
///
/// The passphrase (or API key) is **not** stored in the config file.
/// At server startup the runtime reads `OWS_PASSPHRASE` from the
/// environment, or prompts interactively if a TTY is available.
///
/// ```yaml
/// wallet:
///   name: treasury
///   vault_path: ~/.ows  # optional, defaults to OWS standard path
///   payment:
///     tempo_rpc_url: https://rpc.moderato.tempo.xyz
///     solana_rpc_url: https://api.mainnet-beta.solana.com
/// ```
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WalletConfig {
    /// OWS wallet name (or UUID).
    pub name: String,

    /// Custom OWS vault directory. Defaults to `~/.ows`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub vault_path: Option<String>,

    /// Client-side payment configuration.
    ///
    /// When set, enables automatic 402 Payment Required handling for
    /// providers configured with `auth: mpp`. The wallet signs payment
    /// transactions using Tempo or Solana.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub payment: Option<PaymentClientConfig>,
}

/// Client-side payment configuration for the OWS wallet.
///
/// Controls how the wallet pays upstream providers when they return
/// `402 Payment Required`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PaymentClientConfig {
    /// Tempo RPC URL for session channel operations.
    ///
    /// Required for Tempo session and charge payments.
    /// Defaults to the Moderato testnet if omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tempo_rpc_url: Option<String>,

    /// Solana RPC URL for broadcasting transactions.
    ///
    /// Required for Solana charge payments.
    /// Falls back to `solana_rpc_url` at the top-level config.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_rpc_url: Option<String>,

    /// Maximum session channel deposit in base units.
    ///
    /// Caps the server's `suggestedDeposit` to prevent overspending.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_max_deposit: Option<u128>,

    /// Default session channel deposit in base units.
    ///
    /// Used when the server challenge does not include `suggestedDeposit`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_default_deposit: Option<u128>,
}

/// Authentication configuration.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum AuthConfig {
    /// Standard bearer token (`Authorization: Bearer <key>`).
    Bearer { api_key: String },
    /// Key in a custom header (e.g. `x-api-key`).
    Header {
        header_name: String,
        api_key: String,
    },
    /// x402 payment protocol — requests are paid via a Solana wallet.
    X402,
    /// MPP (Machine Payment Protocol) — requests are paid via an EVM wallet.
    Mpp,
    /// OWS wallet authentication — requests are signed by a local wallet.
    Wallet,
    /// OAuth 2.0 authentication.
    ///
    /// Tokens are acquired interactively via the device code flow (RFC 8628)
    /// and persisted to the token store (`tokens.json`).
    #[serde(rename = "oauth")]
    OAuth {
        /// OAuth grant type (currently only `device_code`).
        grant: OAuthGrant,
        /// OAuth client ID.
        client_id: String,
        /// Requested scopes (space-separated).
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
        /// Device authorization endpoint URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        device_auth_url: Option<String>,
        /// Token endpoint URL.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        token_url: Option<String>,
        /// GitHub domain (defaults to `github.com`).
        ///
        /// For GitHub Enterprise, set this to the enterprise domain
        /// (e.g. `company.ghe.com`). When set, `device_auth_url`,
        /// `token_url`, and `api_base` are derived from the domain
        /// unless explicitly overridden.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        domain: Option<String>,
    },
    /// Extension point for non-standard auth methods.
    Custom {
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
}

/// OAuth grant type.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OAuthGrant {
    /// OAuth 2.0 Device Authorization Grant (RFC 8628).
    DeviceCode,
}

// ── Model routing configuration ──────────────────────────────────────

/// Routing strategy for a model with multiple endpoints.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RoutingStrategy {
    /// Try endpoints in declared order; failover to next on error.
    #[default]
    Priority,
    /// Distribute requests evenly via round-robin.
    LoadBalance,
}

/// A single endpoint that a model or tool can be routed to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Endpoint {
    /// Provider name (must exist in the providers section or built-ins).
    pub provider: String,

    /// Upstream service identifier: model ID for language models, tool ID for tools.
    #[serde(alias = "model_id", alias = "tool_id")]
    pub service_id: String,

    /// Optional per-endpoint API protocol override.
    ///
    /// When set, overrides the provider's default `api_protocol` for this
    /// endpoint only. Useful when a provider speaks multiple protocols.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_protocol: Option<ApiProtocol>,

    /// Optional per-endpoint API key override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// Optional per-endpoint API base override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
}

/// Routing configuration for a virtual model name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub strategy: RoutingStrategy,

    pub endpoints: Vec<Endpoint>,

    /// Human-readable display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,

    /// Maximum input context window in tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_input_tokens: Option<u64>,

    /// Maximum number of output tokens the model can produce.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_output_tokens: Option<u64>,

    /// Input modalities the model accepts.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub input_modalities: Vec<Modality>,

    /// Output modalities the model can produce.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub output_modalities: Vec<Modality>,

    /// Token pricing per million tokens.
    #[serde(default)]
    pub pricing: ModelPricing,
}

// ── Tool routing configuration ──────────────────────────────────────

/// Routing configuration for a virtual tool name.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ToolConfig {
    /// Strategy for selecting among multiple endpoints.
    #[serde(default)]
    pub strategy: RoutingStrategy,

    /// One or more upstream endpoints to route this tool to.
    pub endpoints: Vec<Endpoint>,

    /// Optional per-tool invocation pricing.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pricing: Option<bitrouter_core::pricing::FlatPricing>,

    /// Human-readable description for REST tool discoverability.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,

    /// JSON Schema for input parameters (REST tool discoverability).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub input_schema: Option<serde_json::Value>,

    /// Associated skill name (references a SKILL.md on disk).
    ///
    /// When set, the tool is enriched with skill metadata from the
    /// filesystem skill registry. Skills are a metadata layer — they
    /// do not affect the execution protocol.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill: Option<String>,
}

// ── Content-based auto-routing configuration ────────────────────────

/// Configuration for a content-based auto-routing rule.
///
/// When a request targets the trigger model name (the key in the `routing`
/// map), the router inspects message content to detect keyword signals and
/// estimate complexity, then selects a concrete model from the `models` map.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RoutingRuleConfig {
    /// When `true` (the default), built-in signal definitions are merged
    /// before user-defined signals. User signals with the same name
    /// override the built-in version.
    #[serde(default = "default_true")]
    pub inherit_defaults: bool,

    /// User-defined keyword signals, merged on top of built-ins.
    #[serde(default)]
    pub signals: HashMap<String, SignalConfig>,

    /// Complexity estimation heuristics. When omitted, built-in defaults
    /// are used (if `inherit_defaults` is true).
    #[serde(default)]
    pub complexity: ComplexityConfig,

    /// Maps `signal[.complexity]` → model name.
    ///
    /// Lookup order: `"{signal}.{complexity}"` → `"{signal}"` → `"default"`.
    /// Target model names must exist in the top-level `models` section.
    #[serde(default)]
    pub models: HashMap<String, String>,
}

/// Keyword signal configuration.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SignalConfig {
    /// Keywords to match (case-insensitive substring matching).
    #[serde(default)]
    pub keywords: Vec<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ComplexityConfig {
    /// Keywords that indicate higher complexity.
    #[serde(default)]
    pub high_keywords: Vec<String>,

    /// Character count threshold: messages longer than this are considered
    /// more complex.
    #[serde(default)]
    pub message_length_threshold: Option<usize>,

    /// Turn count threshold: conversations with more turns are considered
    /// more complex.
    #[serde(default)]
    pub turn_count_threshold: Option<usize>,

    /// When `true`, the presence of fenced code blocks increases complexity.
    #[serde(default)]
    pub code_blocks_increase_complexity: bool,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips_through_yaml() {
        let config = BitrouterConfig::default();
        let yaml = serde_saphyr::to_string(&config).unwrap();
        let parsed: BitrouterConfig = serde_saphyr::from_str(&yaml).unwrap();
        assert_eq!(parsed.server.listen, config.server.listen);
    }

    #[test]
    fn load_minimal_yaml() {
        let yaml = r#"
server:
  listen: "127.0.0.1:9090"
providers:
  openai:
    api_key: "sk-test"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        assert_eq!(config.server.listen, "127.0.0.1:9090".parse().unwrap());
        // Should have all builtins + user override merged
        assert!(config.providers.contains_key("openai"));
        assert!(config.providers.contains_key("anthropic"));
        assert_eq!(
            config.providers["openai"].api_key.as_deref(),
            Some("sk-test")
        );
    }

    #[test]
    fn load_with_custom_derived_provider() {
        let yaml = r#"
providers:
  my-company:
    derives: openai
    api_base: "https://api.mycompany.com/v1"
    api_key: "sk-custom"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        let p = &config.providers["my-company"];
        assert_eq!(p.api_protocol, Some(ApiProtocol::Openai)); // inherited
        assert_eq!(p.api_base.as_deref(), Some("https://api.mycompany.com/v1")); // overridden
        assert_eq!(p.api_key.as_deref(), Some("sk-custom"));
        assert!(p.derives.is_none()); // resolved
    }

    #[test]
    fn load_with_model_routing() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
models:
  my-gpt4:
    strategy: load_balance
    endpoints:
      - provider: openai
        model_id: gpt-4o
        api_key: "sk-key-a"
      - provider: openai
        model_id: gpt-4o
        api_key: "sk-key-b"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        let model = &config.models["my-gpt4"];
        assert_eq!(model.strategy, RoutingStrategy::LoadBalance);
        assert_eq!(model.endpoints.len(), 2);
        assert_eq!(model.endpoints[0].api_key.as_deref(), Some("sk-key-a"));
    }

    #[test]
    fn load_with_custom_auth() {
        let yaml = r#"
providers:
  aimo:
    derives: openai
    api_base: "https://api.aimo.network/v1"
    auth:
      type: custom
      method: siwx
      params:
        chain_id: 1
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        let p = &config.providers["aimo"];
        assert!(matches!(p.auth, Some(AuthConfig::Custom { .. })));
        if let Some(AuthConfig::Custom { method, .. }) = &p.auth {
            assert_eq!(method, "siwx");
        }
    }

    #[test]
    fn empty_yaml_gets_full_builtins() {
        let config = BitrouterConfig::load_from_str("{}", None).unwrap();
        assert!(config.providers.contains_key("openai"));
        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("google"));
    }

    #[test]
    fn load_with_provider_model_metadata() {
        let yaml = r#"
providers:
  openai:
    api_key: "sk-test"
    models:
      gpt-4o:
        name: "GPT-4o"
        max_input_tokens: 128000
        max_output_tokens: 16384
        input_modalities: [text, image]
        output_modalities: [text]
        pricing:
          input_tokens:
            no_cache: 2.50
          output_tokens:
            text: 10.00
      gpt-4o-mini:
        name: "GPT-4o Mini"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        let openai = &config.providers["openai"];
        let models = openai.models.as_ref().unwrap();

        let gpt4o = &models["gpt-4o"];
        assert_eq!(gpt4o.name.as_deref(), Some("GPT-4o"));
        assert_eq!(gpt4o.max_input_tokens, Some(128000));
        assert_eq!(gpt4o.max_output_tokens, Some(16384));
        assert_eq!(
            gpt4o.input_modalities,
            vec![Modality::Text, Modality::Image]
        );
        assert_eq!(gpt4o.pricing.input_tokens.no_cache, Some(2.50));
        assert_eq!(gpt4o.pricing.output_tokens.text, Some(10.00));

        let mini = &models["gpt-4o-mini"];
        assert_eq!(mini.name.as_deref(), Some("GPT-4o Mini"));
        assert_eq!(mini.pricing.input_tokens.no_cache, None); // default
    }

    #[test]
    fn derives_inherits_model_catalog() {
        let yaml = r#"
providers:
  my-openai:
    derives: openai
    api_key: "sk-custom"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        let my_openai = &config.providers["my-openai"];
        // Should inherit the built-in openai models catalog
        let models = my_openai.models.as_ref().unwrap();
        assert!(models.contains_key("gpt-4o"));
    }

    #[test]
    fn inherit_defaults_true_by_default() {
        let config = BitrouterConfig::load_from_str("{}", None).unwrap();
        assert!(config.inherit_defaults);
        assert!(config.providers.contains_key("openai"));
        assert!(config.providers.contains_key("bitrouter"));
    }

    #[test]
    fn inherit_defaults_false_excludes_builtins() {
        let yaml = r#"
inherit_defaults: false
providers:
  custom:
    api_protocol: openai
    api_base: "https://custom.example.com/v1"
    api_key: "sk-custom"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        assert!(!config.inherit_defaults);
        assert!(config.providers.contains_key("custom"));
        assert!(!config.providers.contains_key("openai"));
        assert!(!config.providers.contains_key("bitrouter"));
        assert_eq!(config.providers.len(), 1);
    }

    #[test]
    fn load_with_tool_routing() {
        let yaml = r#"
providers:
  github-mcp:
    api_protocol: mcp
    api_base: "https://api.githubcopilot.com/mcp"
    api_key: "ghp-test"
tools:
  create_issue:
    strategy: priority
    endpoints:
      - provider: github-mcp
        tool_id: create_issue
  search_code:
    endpoints:
      - provider: github-mcp
        tool_id: search_code
        api_protocol: mcp
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        // 2 user-defined + 6 built-in exa tools
        assert!(config.tools.len() >= 2);
        assert!(config.tools.contains_key("create_issue"));
        assert!(config.tools.contains_key("search_code"));

        let tool = &config.tools["create_issue"];
        assert_eq!(tool.strategy, RoutingStrategy::Priority);
        assert_eq!(tool.endpoints.len(), 1);
        assert_eq!(tool.endpoints[0].provider, "github-mcp");
        assert_eq!(tool.endpoints[0].service_id, "create_issue");
        assert!(tool.endpoints[0].api_protocol.is_none());

        let search = &config.tools["search_code"];
        assert_eq!(search.endpoints[0].api_protocol, Some(ApiProtocol::Mcp));
    }

    #[test]
    fn full_template_deserializes() {
        let yaml = include_str!("../templates/full.yaml");
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();

        // Server
        assert_eq!(config.server.listen, "127.0.0.1:8787".parse().unwrap());
        assert_eq!(config.server.log_level, "info");

        // Database
        assert!(config.database.url.is_some());

        // Providers: builtins + user-defined
        assert!(config.providers.contains_key("openai"));
        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("google"));
        assert!(config.providers.contains_key("my-proxy"));
        assert!(config.providers.contains_key("custom-llm"));
        assert!(config.providers.contains_key("github-mcp"));
        assert!(config.providers.contains_key("header-auth-provider"));
        assert!(config.providers.contains_key("paid-provider"));

        // Derived provider inherits api_protocol
        let my_proxy = &config.providers["my-proxy"];
        assert_eq!(my_proxy.api_protocol, Some(ApiProtocol::Openai));
        assert!(my_proxy.derives.is_none()); // resolved

        // Custom provider
        let custom = &config.providers["custom-llm"];
        assert_eq!(custom.api_protocol, Some(ApiProtocol::Openai));
        let models = custom.models.as_ref().unwrap();
        assert!(models.contains_key("my-model-7b"));

        // Model routing
        assert_eq!(config.models.len(), 3);
        assert!(config.models.contains_key("smart"));
        assert!(config.models.contains_key("fast"));
        assert!(config.models.contains_key("coding"));
        assert_eq!(config.models["smart"].strategy, RoutingStrategy::Priority);
        assert_eq!(config.models["fast"].strategy, RoutingStrategy::LoadBalance);

        // Tool routing
        assert!(config.tools.contains_key("create_issue"));
        assert!(config.tools.contains_key("web_search"));

        // Guardrails
        assert!(config.guardrails.enabled);
        assert!(!config.guardrails.disabled_patterns.is_empty());
        assert!(!config.guardrails.custom_patterns.is_empty());
        assert!(!config.guardrails.upgoing.is_empty());
        assert!(!config.guardrails.downgoing.is_empty());

        // Wallet
        let wallet = config.wallet.as_ref().unwrap();
        assert_eq!(wallet.name, "my-wallet");
        assert!(wallet.payment.is_some());

        // MPP
        let mpp = config.mpp.as_ref().unwrap();
        assert!(mpp.enabled);
    }

    #[test]
    fn minimal_template_deserializes() {
        let yaml = "";
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();

        // Comments-only YAML deserializes with all defaults
        assert_eq!(config.server.listen, "127.0.0.1:8787".parse().unwrap());
        assert_eq!(config.server.log_level, "info");

        // Builtins merged (inherit_defaults defaults to true)
        assert!(config.inherit_defaults);
        assert!(config.providers.contains_key("openai"));
        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("google"));
        assert!(config.providers.contains_key("bitrouter"));

        // No custom models or tools defined
        assert!(config.models.is_empty());

        // No wallet or MPP
        assert!(config.wallet.is_none());
        assert!(config.mpp.is_none());

        // Guardrails enabled by default
        assert!(config.guardrails.enabled);
    }

    #[test]
    fn empty_string_deserializes() {
        let config = BitrouterConfig::load_from_str("", None).unwrap();

        // All defaults applied
        assert_eq!(config.server.listen, "127.0.0.1:8787".parse().unwrap());
        assert!(config.inherit_defaults);
        assert!(config.providers.contains_key("openai"));
        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("google"));
        assert!(config.models.is_empty());
        assert!(config.guardrails.enabled);
    }

    #[test]
    fn load_with_oauth_auth() {
        let yaml = r#"
providers:
  github-copilot:
    api_protocol: openai
    api_base: "https://api.githubcopilot.com"
    auth:
      type: oauth
      grant: device_code
      client_id: "Iv23limb4eFHH5zfOCr2"
      scope: "read:user"
      device_auth_url: "https://github.com/login/device/code"
      token_url: "https://github.com/login/oauth/access_token"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        let p = &config.providers["github-copilot"];
        assert!(matches!(p.auth, Some(AuthConfig::OAuth { .. })));
        if let Some(AuthConfig::OAuth {
            grant,
            client_id,
            scope,
            device_auth_url,
            token_url,
            ..
        }) = &p.auth
        {
            assert_eq!(*grant, OAuthGrant::DeviceCode);
            assert_eq!(client_id, "Iv23limb4eFHH5zfOCr2");
            assert_eq!(scope.as_deref(), Some("read:user"));
            assert_eq!(
                device_auth_url.as_deref(),
                Some("https://github.com/login/device/code")
            );
            assert_eq!(
                token_url.as_deref(),
                Some("https://github.com/login/oauth/access_token")
            );
        }
    }

    #[test]
    fn load_oauth_with_defaults() {
        let yaml = r#"
providers:
  test-oauth:
    api_protocol: openai
    api_base: "https://api.example.com"
    auth:
      type: oauth
      grant: device_code
      client_id: "test-client-id"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        let p = &config.providers["test-oauth"];
        if let Some(AuthConfig::OAuth {
            scope,
            device_auth_url,
            token_url,
            ..
        }) = &p.auth
        {
            assert!(scope.is_none());
            assert!(device_auth_url.is_none());
            assert!(token_url.is_none());
        } else {
            panic!("expected OAuth auth config");
        }
    }
}
