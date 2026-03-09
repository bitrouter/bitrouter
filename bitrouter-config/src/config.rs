use std::{
    collections::HashMap,
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

    /// Optional path to a `.env` file.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub env_file: Option<String>,

    /// Provider definitions (merged on top of built-in providers).
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,

    /// Model routing definitions.
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,
}

impl BitrouterConfig {
    /// Full config loading pipeline:
    ///
    /// 1. Read and parse YAML
    /// 2. Optionally load a `.env` file
    /// 3. Substitute `${VAR}` references in all string values
    /// 4. Merge user providers on top of the built-in registry
    /// 5. Resolve `derives` chains
    /// 6. Apply `env_prefix` auto-overrides
    pub fn load_from_file(path: &Path) -> crate::error::Result<Self> {
        let raw =
            std::fs::read_to_string(path).map_err(|e| crate::error::ConfigError::ConfigRead {
                path: path.to_path_buf(),
                source: e,
            })?;
        Self::load_from_str(&raw)
    }

    /// Loads from an in-memory YAML string (useful for testing).
    pub fn load_from_str(raw: &str) -> crate::error::Result<Self> {
        // First pass: extract env_file before full substitution
        let pre: PreConfig = serde_yaml::from_str(raw)
            .map_err(|e| crate::error::ConfigError::ConfigParse(e.to_string()))?;

        // Load environment
        let env = load_env(pre.env_file.as_deref());

        // Second pass: substitute env vars in the YAML tree, then deserialize
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

/// Minimal pre-parse to extract `env_file` before full substitution.
#[derive(Deserialize)]
struct PreConfig {
    #[serde(default)]
    env_file: Option<String>,
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
    /// Extension point for non-standard auth methods (e.g. SIWx).
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
        let config = BitrouterConfig::load_from_str(yaml).unwrap();
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
        let config = BitrouterConfig::load_from_str(yaml).unwrap();
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
        let config = BitrouterConfig::load_from_str(yaml).unwrap();
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
        let config = BitrouterConfig::load_from_str(yaml).unwrap();
        let p = &config.providers["aimo"];
        assert!(matches!(p.auth, Some(AuthConfig::Custom { .. })));
        if let Some(AuthConfig::Custom { method, .. }) = &p.auth {
            assert_eq!(method, "siwx");
        }
    }

    #[test]
    fn empty_yaml_gets_full_builtins() {
        let config = BitrouterConfig::load_from_str("{}").unwrap();
        assert!(config.providers.contains_key("openai"));
        assert!(config.providers.contains_key("anthropic"));
        assert!(config.providers.contains_key("google"));
    }
}
