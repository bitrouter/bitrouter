use std::{
    collections::HashMap,
    net::{IpAddr, Ipv4Addr, SocketAddr},
    path::{Path, PathBuf},
};

use serde::{Deserialize, Serialize};

use crate::env::{load_env, substitute_in_value};
use crate::model::{ModelConfig, ProviderConfig};
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

    /// Provider definitions (merged on top of built-in providers).
    #[serde(default)]
    pub providers: HashMap<String, ProviderConfig>,

    /// Model routing definitions.
    #[serde(default)]
    pub models: HashMap<String, ModelConfig>,

    /// MCP upstream server configurations.
    #[serde(default)]
    pub mcp_servers: Vec<crate::tool::ToolServerConfig>,

    /// Named groups of tool servers for access control convenience.
    #[serde(default)]
    pub mcp_groups: crate::tool::ToolServerAccessGroups,

    /// Upstream A2A agent to proxy through the gateway.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub a2a_agent: Option<crate::agent::AgentConfig>,
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::model::{ApiProtocol, AuthConfig, Modality, ModelInfo, RoutingStrategy};

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
    fn load_with_a2a_agent_config() {
        let yaml = r#"
a2a_agent:
  name: "upstream-agent"
  url: "https://agent.example.com"
  headers:
    Authorization: "Bearer tok123"
"#;
        let config = BitrouterConfig::load_from_str(yaml, None).unwrap();
        let agent = config.a2a_agent.as_ref().unwrap();
        assert_eq!(agent.name, "upstream-agent");
        assert_eq!(agent.url, "https://agent.example.com");
        assert_eq!(
            agent.headers.get("Authorization").map(String::as_str),
            Some("Bearer tok123")
        );
        assert!(agent.card_path.is_none());
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
