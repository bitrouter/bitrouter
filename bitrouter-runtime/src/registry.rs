use std::collections::HashMap;

use crate::config::{ApiProtocol, ProviderConfig};

/// Returns the built-in provider registry with defaults for well-known providers.
///
/// Users override these by declaring the same provider name in their config file.
/// Custom providers can `derives` from any of these to inherit settings.
pub fn builtin_providers() -> HashMap<String, ProviderConfig> {
    let mut p = HashMap::new();

    // ── OpenAI-compatible providers ──────────────────────────────

    p.insert(
        "openai".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.openai.com/v1".into()),
            env_prefix: Some("OPENAI".into()),
            ..Default::default()
        },
    );

    p.insert(
        "openrouter".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://openrouter.ai/api/v1".into()),
            env_prefix: Some("OPENROUTER".into()),
            ..Default::default()
        },
    );

    p.insert(
        "deepseek".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.deepseek.com/v1".into()),
            env_prefix: Some("DEEPSEEK".into()),
            ..Default::default()
        },
    );

    p.insert(
        "mistral".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.mistral.ai/v1".into()),
            env_prefix: Some("MISTRAL".into()),
            ..Default::default()
        },
    );

    p.insert(
        "groq".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.groq.com/openai/v1".into()),
            env_prefix: Some("GROQ".into()),
            ..Default::default()
        },
    );

    p.insert(
        "together".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.together.xyz/v1".into()),
            env_prefix: Some("TOGETHER".into()),
            ..Default::default()
        },
    );

    p.insert(
        "fireworks".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.fireworks.ai/inference/v1".into()),
            env_prefix: Some("FIREWORKS".into()),
            ..Default::default()
        },
    );

    p.insert(
        "xai".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.x.ai/v1".into()),
            env_prefix: Some("XAI".into()),
            ..Default::default()
        },
    );

    p.insert(
        "perplexity".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.perplexity.ai".into()),
            env_prefix: Some("PERPLEXITY".into()),
            ..Default::default()
        },
    );

    p.insert(
        "cerebras".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.cerebras.ai/v1".into()),
            env_prefix: Some("CEREBRAS".into()),
            ..Default::default()
        },
    );

    p.insert(
        "sambanova".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://api.sambanova.ai/v1".into()),
            env_prefix: Some("SAMBANOVA".into()),
            ..Default::default()
        },
    );

    // ── Anthropic-protocol providers ────────────────────────────

    p.insert(
        "anthropic".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Anthropic),
            api_base: Some("https://api.anthropic.com".into()),
            env_prefix: Some("ANTHROPIC".into()),
            ..Default::default()
        },
    );

    // ── Google-protocol providers ───────────────────────────────

    p.insert(
        "google".into(),
        ProviderConfig {
            api_protocol: Some(ApiProtocol::Google),
            api_base: Some("https://generativelanguage.googleapis.com".into()),
            env_prefix: Some("GOOGLE".into()),
            ..Default::default()
        },
    );

    p
}

/// Merges a user-provided provider config on top of a base config.
/// Non-`None` user fields overwrite the corresponding base fields.
pub fn merge_provider(base: &mut ProviderConfig, overlay: ProviderConfig) {
    if overlay.derives.is_some() {
        base.derives = overlay.derives;
    }
    if overlay.api_protocol.is_some() {
        base.api_protocol = overlay.api_protocol;
    }
    if overlay.api_base.is_some() {
        base.api_base = overlay.api_base;
    }
    if overlay.api_key.is_some() {
        base.api_key = overlay.api_key;
    }
    if overlay.auth.is_some() {
        base.auth = overlay.auth;
    }
    if overlay.env_prefix.is_some() {
        base.env_prefix = overlay.env_prefix;
    }
    if overlay.default_headers.is_some() {
        base.default_headers = overlay.default_headers;
    }
}

/// Resolves all provider derivation chains and applies `env_prefix` overrides.
///
/// After this call every provider has all inherited fields filled in and
/// `derives` is set to `None`.
pub fn resolve_providers(
    mut providers: HashMap<String, ProviderConfig>,
    env: &HashMap<String, String>,
) -> HashMap<String, ProviderConfig> {
    // Resolve derives chains
    let names: Vec<String> = providers.keys().cloned().collect();
    for name in &names {
        resolve_derives(&mut providers, name, &mut Vec::new());
    }

    // Apply env_prefix auto-overrides (always wins when non-empty)
    for provider in providers.values_mut() {
        if let Some(prefix) = &provider.env_prefix {
            if let Some(key) = env.get(&format!("{prefix}_API_KEY")) {
                if !key.is_empty() {
                    provider.api_key = Some(key.clone());
                }
            }
            if let Some(url) = env.get(&format!("{prefix}_BASE_URL")) {
                if !url.is_empty() {
                    provider.api_base = Some(url.clone());
                }
            }
        }
    }

    providers
}

/// Recursively resolves `derives` for a single provider.
fn resolve_derives(
    providers: &mut HashMap<String, ProviderConfig>,
    name: &str,
    visited: &mut Vec<String>,
) {
    if visited.contains(&name.to_owned()) {
        return; // circular – bail
    }

    let parent_name = match providers.get(name) {
        Some(p) => match &p.derives {
            Some(d) => d.clone(),
            None => return,
        },
        None => return,
    };

    visited.push(name.to_owned());

    // Resolve the parent first
    resolve_derives(providers, &parent_name, visited);

    // Clone parent fields, then overlay child's explicit values
    if let Some(parent) = providers.get(&parent_name).cloned() {
        if let Some(child) = providers.get_mut(name) {
            if child.api_protocol.is_none() {
                child.api_protocol = parent.api_protocol;
            }
            if child.api_base.is_none() {
                child.api_base = parent.api_base;
            }
            if child.api_key.is_none() {
                child.api_key = parent.api_key;
            }
            if child.auth.is_none() {
                child.auth = parent.auth;
            }
            if child.env_prefix.is_none() {
                child.env_prefix = parent.env_prefix;
            }
            if child.default_headers.is_none() {
                child.default_headers = parent.default_headers;
            }
            child.derives = None;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn builtin_registry_contains_core_providers() {
        let providers = builtin_providers();
        assert!(providers.contains_key("openai"));
        assert!(providers.contains_key("anthropic"));
        assert!(providers.contains_key("google"));
        assert_eq!(
            providers["openai"].api_protocol,
            Some(ApiProtocol::Openai)
        );
        assert_eq!(
            providers["anthropic"].api_protocol,
            Some(ApiProtocol::Anthropic)
        );
    }

    #[test]
    fn derives_resolution() {
        let mut providers = HashMap::new();
        providers.insert(
            "base".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Openai),
                api_base: Some("https://base.example.com/v1".into()),
                api_key: Some("base-key".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "child".into(),
            ProviderConfig {
                derives: Some("base".into()),
                api_base: Some("https://child.example.com/v1".into()),
                ..Default::default()
            },
        );

        let env = HashMap::new();
        let resolved = resolve_providers(providers, &env);

        let child = &resolved["child"];
        assert_eq!(child.api_protocol, Some(ApiProtocol::Openai)); // inherited
        assert_eq!(
            child.api_base.as_deref(),
            Some("https://child.example.com/v1")
        ); // overridden
        assert_eq!(child.api_key.as_deref(), Some("base-key")); // inherited
        assert!(child.derives.is_none()); // cleared
    }

    #[test]
    fn chained_derives() {
        let mut providers = HashMap::new();
        providers.insert(
            "root".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Openai),
                api_base: Some("https://root.example.com/v1".into()),
                api_key: Some("root-key".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "mid".into(),
            ProviderConfig {
                derives: Some("root".into()),
                api_base: Some("https://mid.example.com/v1".into()),
                ..Default::default()
            },
        );
        providers.insert(
            "leaf".into(),
            ProviderConfig {
                derives: Some("mid".into()),
                api_key: Some("leaf-key".into()),
                ..Default::default()
            },
        );

        let env = HashMap::new();
        let resolved = resolve_providers(providers, &env);

        let leaf = &resolved["leaf"];
        assert_eq!(leaf.api_protocol, Some(ApiProtocol::Openai)); // from root
        assert_eq!(
            leaf.api_base.as_deref(),
            Some("https://mid.example.com/v1")
        ); // from mid
        assert_eq!(leaf.api_key.as_deref(), Some("leaf-key")); // own value
    }

    #[test]
    fn env_prefix_overrides() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Openai),
                api_base: Some("https://api.openai.com/v1".into()),
                env_prefix: Some("OPENAI".into()),
                ..Default::default()
            },
        );

        let env = HashMap::from([
            ("OPENAI_API_KEY".into(), "sk-from-env".into()),
            (
                "OPENAI_BASE_URL".into(),
                "https://proxy.example.com/v1".into(),
            ),
        ]);
        let resolved = resolve_providers(providers, &env);

        let openai = &resolved["openai"];
        assert_eq!(openai.api_key.as_deref(), Some("sk-from-env"));
        assert_eq!(
            openai.api_base.as_deref(),
            Some("https://proxy.example.com/v1")
        );
    }

    #[test]
    fn env_prefix_empty_value_does_not_override() {
        let mut providers = HashMap::new();
        providers.insert(
            "openai".into(),
            ProviderConfig {
                api_protocol: Some(ApiProtocol::Openai),
                api_base: Some("https://api.openai.com/v1".into()),
                api_key: Some("existing-key".into()),
                env_prefix: Some("OPENAI".into()),
                ..Default::default()
            },
        );

        let env = HashMap::from([("OPENAI_API_KEY".into(), "".into())]);
        let resolved = resolve_providers(providers, &env);

        // Empty env var should NOT override the existing key
        assert_eq!(resolved["openai"].api_key.as_deref(), Some("existing-key"));
    }

    #[test]
    fn merge_provider_partial_overlay() {
        let mut base = ProviderConfig {
            api_protocol: Some(ApiProtocol::Openai),
            api_base: Some("https://base.com/v1".into()),
            api_key: Some("base-key".into()),
            ..Default::default()
        };
        let overlay = ProviderConfig {
            api_key: Some("overlay-key".into()),
            ..Default::default()
        };
        merge_provider(&mut base, overlay);

        assert_eq!(base.api_base.as_deref(), Some("https://base.com/v1")); // kept
        assert_eq!(base.api_key.as_deref(), Some("overlay-key")); // overridden
    }
}
