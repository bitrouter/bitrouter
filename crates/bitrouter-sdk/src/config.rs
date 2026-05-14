//! Configuration types and loader — gated behind the `config_file` feature.
//!
//! The config file (`bitrouter.yaml`) serves the `bitrouter` CLI app. Core
//! fields live at the top level (`server` / `database` / `providers` /
//! `models` / `presets` / `variants`); plugin config lives under `plugins`.
//! See design doc 003 §10.
//!
//! Phase 2 ships the type surface, `${VAR}` substitution and a `serde-saphyr`
//! loader. The registry-style provider schema (glob-prefix `api_protocol` /
//! `rate_limits`) and the routing tables are fleshed out in Phase 4.

use std::collections::HashMap;

use serde::Deserialize;

use crate::error::{BitrouterError, Result};

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
    /// Optional explicit virtual-model definitions (Strategy 2; see 003 §5.2).
    /// Kept as raw JSON in Phase 2; Phase 4 gives it a typed schema.
    pub models: HashMap<String, serde_json::Value>,
    /// `@preset` definitions (003 §5.4). Raw JSON until Phase 4.
    pub presets: HashMap<String, serde_json::Value>,
    /// `:variant` definitions (003 §5.4). Raw JSON until Phase 4.
    pub variants: HashMap<String, serde_json::Value>,
    /// Plugin config, keyed by plugin / bundle id.
    pub plugins: HashMap<String, serde_json::Value>,
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
            inherit_defaults: true,
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
    /// config file produced by `bitrouter init` writes `true` (003 §10).
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

/// One upstream provider entry.
#[derive(Debug, Clone, Deserialize)]
#[serde(default)]
pub struct ProviderConfig {
    /// Upstream API base URL.
    pub api_base: String,
    /// Upstream API key (often a `${VAR}` reference).
    pub api_key: String,
    /// Default wire protocol for this provider's models. Phase 4 promotes this
    /// to a glob-prefix pattern list; Phase 2 keeps a single default.
    pub api_protocol: Option<String>,
    /// Declared model ids. Empty + `auto_discover` triggers discovery (003 §5.6).
    pub models: Vec<ProviderModel>,
    /// When `true` and `models` is empty, discover models from the provider.
    pub auto_discover: bool,
    /// Whether this provider is active / routable.
    pub active: bool,
}

impl Default for ProviderConfig {
    fn default() -> Self {
        Self {
            api_base: String::new(),
            api_key: String::new(),
            api_protocol: None,
            models: Vec::new(),
            auto_discover: false,
            active: true,
        }
    }
}

/// One model entry under a provider.
#[derive(Debug, Clone, Deserialize)]
pub struct ProviderModel {
    /// Model id at the provider.
    pub id: String,
    /// Per-model protocol override.
    #[serde(default)]
    pub api_protocol: Option<String>,
}

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
                // a `${` with no closing brace — emit verbatim and stop.
                out.push_str("${");
                rest = after;
            }
        }
    }
    out.push_str(rest);
    Ok(out)
}

/// Replace every `${VAR}` occurrence with the value of environment variable
/// `VAR`. An undefined variable is an error (config F8: `${VAR}` substitution).
pub fn substitute_env(input: &str) -> Result<String> {
    substitute_with(input, |name| std::env::var(name).ok())
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
    serde_saphyr::from_str(&substituted)
        .map_err(|e| BitrouterError::bad_request(format!("invalid bitrouter.yaml: {e}")))
}

/// Load and parse `bitrouter.yaml` from disk.
pub async fn load(path: impl AsRef<std::path::Path>) -> Result<Config> {
    let path = path.as_ref();
    let raw = tokio::fs::read_to_string(path)
        .await
        .map_err(|e| BitrouterError::internal(format!("reading config {}: {e}", path.display())))?;
    parse(&raw)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let cfg = Config::default();
        assert_eq!(cfg.server.listen, "0.0.0.0:4356");
        assert!(
            !cfg.server.skip_auth,
            "skip_auth code default must be false"
        );
        assert!(cfg.inherit_defaults);
    }

    #[test]
    fn env_substitution_replaces_vars() {
        let out = substitute_with("api_key: ${BR_TEST_KEY}", |n| {
            (n == "BR_TEST_KEY").then(|| "secret-123".to_string())
        })
        .unwrap();
        assert_eq!(out, "api_key: secret-123");
    }

    #[test]
    fn env_substitution_errors_on_undefined() {
        let err = substitute_with("k: ${MISSING}", |_| None).unwrap_err();
        assert_eq!(err.status(), 400);
    }

    #[test]
    fn env_substitution_handles_multiple_and_literals() {
        let out = substitute_with("a=${A} b=${B} c", |n| Some(format!("<{n}>"))).unwrap();
        assert_eq!(out, "a=<A> b=<B> c");
        // an unterminated `${` is emitted verbatim
        assert_eq!(substitute_with("x ${oops", |_| None).unwrap(), "x ${oops");
    }

    #[test]
    fn parses_minimal_config() {
        let yaml = r#"
server:
  listen: "127.0.0.1:9000"
  skip_auth: true
providers:
  openai:
    api_base: https://api.openai.com/v1
    api_key: ${BR_CFG_KEY}
    models:
      - id: gpt-5
"#;
        let cfg = parse_with(yaml, |n| (n == "BR_CFG_KEY").then(|| "k-abc".to_string())).unwrap();
        assert_eq!(cfg.server.listen, "127.0.0.1:9000");
        assert!(cfg.server.skip_auth);
        let openai = cfg.providers.get("openai").unwrap();
        assert_eq!(openai.api_key, "k-abc");
        assert_eq!(openai.models[0].id, "gpt-5");
    }
}
