//! Agent routing configuration engine.
//!
//! Resolves `${VAR}` placeholders in agent routing definitions and applies
//! config-file patches to redirect agent LLM traffic through BitRouter.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use crate::config::{AgentRouting, ConfigFileFormat, ConfigFilePatch};

/// Context for resolving `${VAR}` placeholders in routing values.
pub struct RoutingContext {
    vars: HashMap<String, String>,
}

impl RoutingContext {
    /// Build a routing context from BitRouter's runtime state.
    ///
    /// Populates `BITROUTER_URL`, `BITROUTER_URL_V1`, and any provider
    /// API keys from the loaded config.
    pub fn new(listen_addr: &str, provider_keys: &HashMap<String, String>) -> Self {
        let bitrouter_url = format!("http://{listen_addr}");
        let bitrouter_url_v1 = format!("{bitrouter_url}/v1");

        let mut vars = HashMap::new();
        vars.insert("BITROUTER_URL".to_owned(), bitrouter_url);
        vars.insert("BITROUTER_URL_V1".to_owned(), bitrouter_url_v1);

        for (key, value) in provider_keys {
            vars.insert(key.clone(), value.clone());
        }

        Self { vars }
    }

    /// Substitute `${VAR}` references in a string value.
    ///
    /// Unknown variables resolve to empty string.
    pub fn substitute(&self, input: &str) -> String {
        let mut result = String::with_capacity(input.len());
        let mut chars = input.chars().peekable();

        while let Some(ch) = chars.next() {
            if ch == '$' && chars.peek() == Some(&'{') {
                chars.next(); // consume '{'
                let mut var_name = String::new();
                for c in chars.by_ref() {
                    if c == '}' {
                        break;
                    }
                    var_name.push(c);
                }
                if let Some(val) = self.vars.get(&var_name) {
                    result.push_str(val);
                }
            } else {
                result.push(ch);
            }
        }

        result
    }

    /// Substitute variables in a JSON value recursively.
    fn substitute_json(&self, value: &serde_json::Value) -> serde_json::Value {
        match value {
            serde_json::Value::String(s) => serde_json::Value::String(self.substitute(s)),
            serde_json::Value::Array(arr) => {
                serde_json::Value::Array(arr.iter().map(|v| self.substitute_json(v)).collect())
            }
            serde_json::Value::Object(map) => {
                let new_map = map
                    .iter()
                    .map(|(k, v)| (k.clone(), self.substitute_json(v)))
                    .collect();
                serde_json::Value::Object(new_map)
            }
            other => other.clone(),
        }
    }

    /// Resolve the env vars for an agent's routing config.
    ///
    /// Returns a map of env var name → resolved value, ready for
    /// injection into a subprocess.
    pub fn resolve_env(&self, routing: &AgentRouting) -> HashMap<String, String> {
        routing
            .env
            .iter()
            .map(|(k, v)| (k.clone(), self.substitute(v)))
            .filter(|(_, v)| !v.is_empty())
            .collect()
    }

    /// Apply all config-file patches for an agent.
    ///
    /// Returns a list of `(path, result)` for each patch attempted.
    pub fn apply_config_patches(
        &self,
        patches: &[ConfigFilePatch],
    ) -> Vec<(PathBuf, Result<(), String>)> {
        patches
            .iter()
            .map(|patch| {
                let path = expand_tilde(&patch.path);
                let result = self.apply_single_patch(&path, patch);
                (path, result)
            })
            .collect()
    }

    fn apply_single_patch(&self, path: &Path, patch: &ConfigFilePatch) -> Result<(), String> {
        match patch.format {
            ConfigFileFormat::Json => self.apply_json_patch(path, &patch.values),
            ConfigFileFormat::Toml => self.apply_toml_patch(path, &patch.values),
        }
    }

    fn apply_json_patch(
        &self,
        path: &Path,
        values: &HashMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        // Read existing file or start with empty object
        let mut doc: serde_json::Value = if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            serde_json::from_str(&content).map_err(|e| format!("parse {}: {e}", path.display()))?
        } else {
            serde_json::Value::Object(serde_json::Map::new())
        };

        // Apply each key-value pair using dot-notation
        for (key, value) in values {
            let resolved = self.substitute_json(value);
            set_json_path(&mut doc, key, resolved);
        }

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }

        let output = serde_json::to_string_pretty(&doc).map_err(|e| format!("serialize: {e}"))?;
        std::fs::write(path, output).map_err(|e| format!("write {}: {e}", path.display()))?;

        Ok(())
    }

    fn apply_toml_patch(
        &self,
        path: &Path,
        values: &HashMap<String, serde_json::Value>,
    ) -> Result<(), String> {
        // Read existing file or start fresh
        let mut doc: toml_edit::DocumentMut = if path.exists() {
            let content = std::fs::read_to_string(path)
                .map_err(|e| format!("read {}: {e}", path.display()))?;
            content
                .parse()
                .map_err(|e| format!("parse {}: {e}", path.display()))?
        } else {
            toml_edit::DocumentMut::new()
        };

        // Apply each dot-notation key
        for (key, value) in values {
            let resolved = self.substitute_json(value);
            set_toml_path(&mut doc, key, &resolved);
        }

        // Ensure parent directory exists
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .map_err(|e| format!("mkdir {}: {e}", parent.display()))?;
        }

        std::fs::write(path, doc.to_string())
            .map_err(|e| format!("write {}: {e}", path.display()))?;

        Ok(())
    }
}

/// Expand `~` prefix to the user's home directory.
fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(home) = dirs::home_dir()
    {
        return home.join(rest);
    }
    PathBuf::from(path)
}

/// Set a value at a dot-notation path in a JSON document.
///
/// Creates intermediate objects as needed.
/// Example: `set_json_path(doc, "a.b.c", val)` sets `doc["a"]["b"]["c"] = val`.
fn set_json_path(doc: &mut serde_json::Value, path: &str, value: serde_json::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current = doc;

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Last part: set the value
            if let serde_json::Value::Object(map) = current {
                map.insert((*part).to_owned(), value);
                return;
            }
        } else {
            // Intermediate: ensure object exists
            if !current.get(*part).is_some_and(|v| v.is_object())
                && let serde_json::Value::Object(map) = current
            {
                map.insert(
                    (*part).to_owned(),
                    serde_json::Value::Object(serde_json::Map::new()),
                );
            }
            if let serde_json::Value::Object(map) = current {
                if let Some(next) = map.get_mut(*part) {
                    current = next;
                } else {
                    return; // Should not happen since we just inserted
                }
            } else {
                return; // Can't traverse non-object
            }
        }
    }
}

/// Set a value at a dot-notation path in a TOML document.
fn set_toml_path(doc: &mut toml_edit::DocumentMut, path: &str, value: &serde_json::Value) {
    let parts: Vec<&str> = path.split('.').collect();
    let mut current: &mut toml_edit::Item = doc.as_item_mut();

    for (i, part) in parts.iter().enumerate() {
        if i == parts.len() - 1 {
            // Last part: set the value
            if let Some(table) = current.as_table_like_mut() {
                table.insert(part, json_to_toml_item(value));
            }
        } else {
            // Intermediate: ensure table exists
            if current.get(part).is_none_or(|v| !v.is_table_like())
                && let Some(table) = current.as_table_like_mut()
            {
                table.insert(part, toml_edit::Item::Table(toml_edit::Table::new()));
            }
            if let Some(table) = current.as_table_like_mut() {
                if let Some(next) = table.get_mut(part) {
                    current = next;
                } else {
                    return;
                }
            } else {
                return;
            }
        }
    }
}

/// Convert a JSON value to a TOML item.
fn json_to_toml_item(value: &serde_json::Value) -> toml_edit::Item {
    match value {
        serde_json::Value::String(s) => toml_edit::Item::Value(toml_edit::Value::from(s.as_str())),
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                toml_edit::Item::Value(toml_edit::Value::from(i))
            } else if let Some(f) = n.as_f64() {
                toml_edit::Item::Value(toml_edit::Value::from(f))
            } else {
                toml_edit::Item::None
            }
        }
        serde_json::Value::Bool(b) => toml_edit::Item::Value(toml_edit::Value::from(*b)),
        serde_json::Value::Object(map) => {
            let mut table = toml_edit::Table::new();
            for (k, v) in map {
                table.insert(k, json_to_toml_item(v));
            }
            toml_edit::Item::Table(table)
        }
        serde_json::Value::Array(arr) => {
            let mut array = toml_edit::Array::new();
            for v in arr {
                if let toml_edit::Item::Value(val) = json_to_toml_item(v) {
                    array.push(val);
                }
            }
            toml_edit::Item::Value(toml_edit::Value::Array(array))
        }
        serde_json::Value::Null => toml_edit::Item::None,
    }
}

/// Extract provider API keys from a `BitrouterConfig` for routing context.
///
/// Returns a map of common env var names (e.g. `OPENAI_API_KEY`) to their values.
pub fn extract_provider_keys(
    providers: &HashMap<String, crate::config::ProviderConfig>,
) -> HashMap<String, String> {
    let mut keys = HashMap::new();

    for (name, provider) in providers {
        if let Some(ref api_key) = provider.api_key {
            // Map provider name to standard env var names
            let env_key = match name.as_str() {
                "openai" => "OPENAI_API_KEY",
                "anthropic" => "ANTHROPIC_API_KEY",
                "google" => "GOOGLE_API_KEY",
                "deepseek" => "DEEPSEEK_API_KEY",
                "openrouter" => "OPENROUTER_API_KEY",
                "mistral" => "MISTRAL_API_KEY",
                _ => {
                    // Use env_prefix if available, otherwise uppercase convention
                    if let Some(ref prefix) = provider.env_prefix {
                        // Store as "{PREFIX}_API_KEY" but we need to own the string
                        let key = format!("{prefix}_API_KEY");
                        keys.insert(key, api_key.clone());
                        continue;
                    }
                    continue;
                }
            };
            keys.insert(env_key.to_owned(), api_key.clone());
        }
    }

    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn substitute_basic() {
        let ctx = RoutingContext::new("127.0.0.1:8787", &HashMap::new());
        assert_eq!(
            ctx.substitute("${BITROUTER_URL_V1}"),
            "http://127.0.0.1:8787/v1"
        );
        assert_eq!(ctx.substitute("${BITROUTER_URL}"), "http://127.0.0.1:8787");
    }

    #[test]
    fn substitute_with_provider_keys() {
        let mut keys = HashMap::new();
        keys.insert("OPENAI_API_KEY".to_owned(), "sk-test".to_owned());

        let ctx = RoutingContext::new("127.0.0.1:8787", &keys);
        assert_eq!(ctx.substitute("${OPENAI_API_KEY}"), "sk-test");
    }

    #[test]
    fn substitute_unknown_var_becomes_empty() {
        let ctx = RoutingContext::new("127.0.0.1:8787", &HashMap::new());
        assert_eq!(ctx.substitute("${UNKNOWN}"), "");
    }

    #[test]
    fn resolve_env_filters_empty() {
        let ctx = RoutingContext::new("127.0.0.1:8787", &HashMap::new());
        let routing = AgentRouting {
            env: HashMap::from([
                (
                    "OPENAI_BASE_URL".to_owned(),
                    "${BITROUTER_URL_V1}".to_owned(),
                ),
                ("OPENAI_API_KEY".to_owned(), "${OPENAI_API_KEY}".to_owned()),
            ]),
            config_files: Vec::new(),
        };
        let resolved = ctx.resolve_env(&routing);
        // OPENAI_BASE_URL resolves, OPENAI_API_KEY is empty (not in context)
        assert_eq!(
            resolved.get("OPENAI_BASE_URL").map(String::as_str),
            Some("http://127.0.0.1:8787/v1")
        );
        assert!(!resolved.contains_key("OPENAI_API_KEY"));
    }

    #[test]
    fn set_json_path_nested() {
        let mut doc = serde_json::json!({});
        set_json_path(&mut doc, "a.b.c", serde_json::Value::String("hello".into()));
        assert_eq!(doc["a"]["b"]["c"], "hello");
    }

    #[test]
    fn set_json_path_preserves_existing() {
        let mut doc = serde_json::json!({"a": {"existing": 1}});
        set_json_path(
            &mut doc,
            "a.new_key",
            serde_json::Value::String("val".into()),
        );
        assert_eq!(doc["a"]["existing"], 1);
        assert_eq!(doc["a"]["new_key"], "val");
    }

    #[test]
    fn apply_json_patch_creates_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.json");

        let ctx = RoutingContext::new("127.0.0.1:8787", &HashMap::new());
        ctx.apply_json_patch(
            &path,
            &HashMap::from([(
                "baseUrl".to_owned(),
                serde_json::Value::String("${BITROUTER_URL_V1}".to_owned()),
            )]),
        )
        .map_err(|e| e.to_string())?;

        let raw = std::fs::read_to_string(&path)?;
        let content: serde_json::Value = serde_json::from_str(&raw)?;
        assert_eq!(content["baseUrl"], "http://127.0.0.1:8787/v1");
        Ok(())
    }

    #[test]
    fn apply_toml_patch_creates_file() -> Result<(), Box<dyn std::error::Error>> {
        let dir = tempfile::tempdir()?;
        let path = dir.path().join("test.toml");

        let ctx = RoutingContext::new("127.0.0.1:8787", &HashMap::new());
        ctx.apply_toml_patch(
            &path,
            &HashMap::from([(
                "providers.openai.api_base".to_owned(),
                serde_json::Value::String("${BITROUTER_URL_V1}".to_owned()),
            )]),
        )
        .map_err(|e| e.to_string())?;

        let raw = std::fs::read_to_string(&path)?;
        let doc: toml_edit::DocumentMut = raw.parse()?;
        assert_eq!(
            doc["providers"]["openai"]["api_base"].as_str(),
            Some("http://127.0.0.1:8787/v1")
        );
        Ok(())
    }

    #[test]
    fn extract_provider_keys_standard() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".to_owned(),
            crate::config::ProviderConfig {
                api_key: Some("sk-openai".to_owned()),
                ..Default::default()
            },
        );
        providers.insert(
            "anthropic".to_owned(),
            crate::config::ProviderConfig {
                api_key: Some("sk-ant".to_owned()),
                ..Default::default()
            },
        );

        let keys = extract_provider_keys(&providers);
        assert_eq!(
            keys.get("OPENAI_API_KEY").map(String::as_str),
            Some("sk-openai")
        );
        assert_eq!(
            keys.get("ANTHROPIC_API_KEY").map(String::as_str),
            Some("sk-ant")
        );
    }
}
