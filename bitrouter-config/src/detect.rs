use std::collections::HashMap;

use crate::registry::builtin_provider_defs;

/// A provider detected from environment variables.
#[derive(Debug, Clone)]
pub struct DetectedProvider {
    /// Provider name (e.g. "openai").
    pub name: String,
    /// Environment variable prefix (e.g. "OPENAI").
    pub env_prefix: String,
    /// The API key variable name (e.g. "OPENAI_API_KEY").
    pub api_key_var: String,
    /// Whether `{PREFIX}_BASE_URL` is also set.
    pub has_base_url: bool,
}

/// Scan an environment map for builtin providers that have API keys set.
///
/// Accepts an explicit env map for testability.
pub fn detect_providers(env: &HashMap<String, String>) -> Vec<DetectedProvider> {
    let defs = builtin_provider_defs();
    let mut detected: Vec<DetectedProvider> = defs
        .iter()
        .filter_map(|(name, bp)| {
            let prefix = bp.config.env_prefix.as_deref()?;
            let key_var = format!("{prefix}_API_KEY");
            let key_value = env.get(&key_var).map(|v| v.as_str()).unwrap_or("");
            if key_value.is_empty() {
                return None;
            }
            let base_var = format!("{prefix}_BASE_URL");
            let has_base_url = env
                .get(&base_var)
                .map(|v| !v.is_empty())
                .unwrap_or(false);
            Some(DetectedProvider {
                name: name.clone(),
                env_prefix: prefix.to_owned(),
                api_key_var: key_var,
                has_base_url,
            })
        })
        .collect();
    detected.sort_by(|a, b| a.name.cmp(&b.name));
    detected
}

/// Convenience wrapper using the process environment.
pub fn detect_providers_from_env() -> Vec<DetectedProvider> {
    let env: HashMap<String, String> = std::env::vars().collect();
    detect_providers(&env)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_provider_with_key() {
        let env = HashMap::from([("OPENAI_API_KEY".into(), "sk-test".into())]);
        let detected = detect_providers(&env);
        assert_eq!(detected.len(), 1);
        assert_eq!(detected[0].name, "openai");
        assert_eq!(detected[0].api_key_var, "OPENAI_API_KEY");
        assert!(!detected[0].has_base_url);
    }

    #[test]
    fn detects_base_url() {
        let env = HashMap::from([
            ("OPENAI_API_KEY".into(), "sk-test".into()),
            ("OPENAI_BASE_URL".into(), "https://proxy.com/v1".into()),
        ]);
        let detected = detect_providers(&env);
        assert!(detected[0].has_base_url);
    }

    #[test]
    fn empty_key_not_detected() {
        let env = HashMap::from([("OPENAI_API_KEY".into(), "".into())]);
        let detected = detect_providers(&env);
        assert!(detected.is_empty());
    }

    #[test]
    fn no_keys_returns_empty() {
        let env = HashMap::new();
        let detected = detect_providers(&env);
        assert!(detected.is_empty());
    }

    #[test]
    fn multiple_providers() {
        let env = HashMap::from([
            ("OPENAI_API_KEY".into(), "sk-test".into()),
            ("ANTHROPIC_API_KEY".into(), "sk-ant-test".into()),
            ("GOOGLE_API_KEY".into(), "goog-test".into()),
        ]);
        let detected = detect_providers(&env);
        assert_eq!(detected.len(), 3);
        // sorted alphabetically
        assert_eq!(detected[0].name, "anthropic");
        assert_eq!(detected[1].name, "google");
        assert_eq!(detected[2].name, "openai");
    }
}
