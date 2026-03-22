use std::{
    collections::HashMap,
    fmt,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::env::{load_env, substitute_in_value};
use crate::registry::{builtin_providers, merge_provider, resolve_providers};

// ── Top-level configuration ──────────────────────────────────────────

/// Root configuration file, typically `bitrouter.yaml`.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct BitrouterConfig {
    #[serde(default)]
    pub server: ServerConfig,

    /// Database configuration.
    #[serde(default)]
    pub database: DatabaseConfig,

    /// Guardrails configuration — local firewall for AI agent traffic.
    #[serde(default)]
    pub guardrails: bitrouter_guardrails::GuardrailConfig,

    /// Solana RPC endpoint used for Swig wallet operations.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub solana_rpc_url: Option<String>,

    /// MPP (Machine Payment Protocol) configuration.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mpp: Option<MppConfig>,

    /// Provider definitions (merged on top of built-in providers).
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,

    /// Model routing definitions.
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
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

        // Substitute env vars in the YAML tree, then deserialize
        let yaml_value: serde_yaml::Value = serde_yaml::from_str(raw)
            .map_err(|e| crate::error::ConfigError::ConfigParse(e.to_string()))?;
        let substituted = substitute_in_value(yaml_value, &env);
        let mut config: BitrouterConfig = serde_yaml::from_value(substituted)
            .map_err(|e| crate::error::ConfigError::ConfigParse(e.to_string()))?;

        // Merge built-in providers with user overrides
        let mut providers = builtin_providers();
        for (name, user_provider) in config.providers.drain() {
            if let Some(existing) = providers.get_mut(&name) {
                merge_provider(existing, user_provider);
            } else {
                providers.insert(name, user_provider);
            }
        }

        // Resolve derives + env_prefix
        config.providers = resolve_providers(providers, &env);

        Ok(config)
    }
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

/// The API protocol / adapter that a provider uses.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiProtocol {
    Openai,
    Anthropic,
    Google,
}

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

/// Token pricing per million tokens for a model.
///
/// Field names mirror the sub-category fields of `LanguageModelInputTokens`
/// and `LanguageModelOutputTokens` from `bitrouter-core` for cross-provider
/// compatibility. Defaults to `0.0` for all fields.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelPricing {
    #[serde(default)]
    pub input_tokens: InputTokenPricing,
    #[serde(default)]
    pub output_tokens: OutputTokenPricing,
}

/// Input token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputTokenPricing {
    /// Cost per million non-cached input tokens.
    #[serde(default)]
    pub no_cache: f64,
    /// Cost per million cache-read input tokens.
    #[serde(default)]
    pub cache_read: f64,
    /// Cost per million cache-write input tokens.
    #[serde(default)]
    pub cache_write: f64,
}

/// Output token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputTokenPricing {
    /// Cost per million text output tokens.
    #[serde(default)]
    pub text: f64,
    /// Cost per million reasoning output tokens.
    #[serde(default)]
    pub reasoning: f64,
}

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
    #[cfg(feature = "mpp-solana")]
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
}

/// Solana-specific MPP configuration.
#[cfg(feature = "mpp-solana")]
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SolanaMppConfig {
    /// Recipient address for payments (Solana base58 pubkey, required).
    pub recipient: String,

    /// Channel (escrow) program address (required for session support).
    pub channel_program: String,

    /// Solana network name (e.g., "mainnet-beta", "devnet").
    #[serde(default = "default_solana_network")]
    pub network: String,
}

#[cfg(feature = "mpp-solana")]
fn default_solana_network() -> String {
    "mainnet-beta".into()
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
    /// Extension point for non-standard auth methods.
    Custom {
        method: String,
        #[serde(default)]
        params: serde_json::Value,
    },
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

/// A single endpoint that a model can be routed to.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelEndpoint {
    /// Provider name (must exist in the providers section or built-ins).
    pub provider: String,

    /// The upstream model ID to send to this provider.
    pub model_id: String,

    /// Optional per-endpoint API key override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,

    /// Optional per-endpoint API base override.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
}

/// Routing configuration for a virtual model name.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelConfig {
    #[serde(default)]
    pub strategy: RoutingStrategy,

    pub endpoints: Vec<ModelEndpoint>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_round_trips_through_yaml() {
        let config = BitrouterConfig::default();
        let yaml = serde_yaml::to_string(&config).unwrap();
        let parsed: BitrouterConfig = serde_yaml::from_str(&yaml).unwrap();
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
    fn model_info_round_trips_through_yaml() {
        let yaml = r#"
name: "GPT-4o"
description: "Multimodal flagship model"
max_input_tokens: 128000
max_output_tokens: 16384
input_modalities:
  - text
  - image
output_modalities:
  - text
pricing:
  input_tokens:
    no_cache: 2.50
    cache_read: 1.25
    cache_write: 2.50
  output_tokens:
    text: 10.00
    reasoning: 10.00
"#;
        let info: ModelInfo = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(info.name.as_deref(), Some("GPT-4o"));
        assert_eq!(
            info.description.as_deref(),
            Some("Multimodal flagship model")
        );
        assert_eq!(info.max_input_tokens, Some(128000));
        assert_eq!(info.max_output_tokens, Some(16384));
        assert_eq!(info.input_modalities, vec![Modality::Text, Modality::Image]);
        assert_eq!(info.output_modalities, vec![Modality::Text]);
        assert_eq!(info.pricing.input_tokens.no_cache, 2.50);
        assert_eq!(info.pricing.input_tokens.cache_read, 1.25);
        assert_eq!(info.pricing.input_tokens.cache_write, 2.50);
        assert_eq!(info.pricing.output_tokens.text, 10.00);
        assert_eq!(info.pricing.output_tokens.reasoning, 10.00);

        // Round-trip
        let serialized = serde_yaml::to_string(&info).unwrap();
        let deserialized: ModelInfo = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.name, info.name);
        assert_eq!(deserialized.pricing.input_tokens.no_cache, 2.50);

        // Legacy "context_length" alias still works
        let legacy_yaml = "context_length: 200000";
        let legacy: ModelInfo = serde_yaml::from_str(legacy_yaml).unwrap();
        assert_eq!(legacy.max_input_tokens, Some(200000));
    }

    #[test]
    fn empty_model_info_deserializes_to_defaults() {
        let info: ModelInfo = serde_yaml::from_str("{}").unwrap();
        assert!(info.name.is_none());
        assert!(info.description.is_none());
        assert!(info.max_input_tokens.is_none());
        assert!(info.max_output_tokens.is_none());
        assert!(info.input_modalities.is_empty());
        assert!(info.output_modalities.is_empty());
        assert_eq!(info.pricing.input_tokens.no_cache, 0.0);
        assert_eq!(info.pricing.output_tokens.text, 0.0);
    }

    #[test]
    fn file_modality_round_trips_through_yaml() {
        let yaml = r#"
input_modalities:
  - text
  - file
output_modalities:
  - text
  - file
"#;
        let info: ModelInfo = serde_yaml::from_str(yaml).unwrap();
        assert_eq!(info.input_modalities, vec![Modality::Text, Modality::File]);
        assert_eq!(info.output_modalities, vec![Modality::Text, Modality::File]);

        let serialized = serde_yaml::to_string(&info).unwrap();
        let deserialized: ModelInfo = serde_yaml::from_str(&serialized).unwrap();
        assert_eq!(deserialized.input_modalities, info.input_modalities);
        assert_eq!(deserialized.output_modalities, info.output_modalities);
    }

    #[test]
    fn modality_display_uses_lowercase_words() {
        assert_eq!(Modality::Text.to_string(), "text");
        assert_eq!(Modality::Image.to_string(), "image");
        assert_eq!(Modality::Audio.to_string(), "audio");
        assert_eq!(Modality::Video.to_string(), "video");
        assert_eq!(Modality::File.to_string(), "file");
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
        assert_eq!(gpt4o.pricing.input_tokens.no_cache, 2.50);
        assert_eq!(gpt4o.pricing.output_tokens.text, 10.00);

        let mini = &models["gpt-4o-mini"];
        assert_eq!(mini.name.as_deref(), Some("GPT-4o Mini"));
        assert_eq!(mini.pricing.input_tokens.no_cache, 0.0); // default
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
}
