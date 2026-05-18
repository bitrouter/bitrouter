//! Fill empty `ProviderConfig` fields from the matching built-in entry.
//!
//! Lets a user write the minimum `providers: { openai: {} }` in their
//! `bitrouter.yaml` and get a fully-populated provider — `api_base`,
//! `api_protocol`, and `api_key` (resolved from the env var the built-in
//! entry advertises) are all filled by [`apply_builtin_defaults`].
//!
//! Opt-out: set `inherit_defaults: false` at the top level of the config.

use std::env;

use bitrouter_sdk::config::{Config, Pattern, PatternMap};
use bitrouter_sdk::language_model::types::ApiProtocol;

use crate::builtin;
use crate::entry::ProtocolMapping;

/// Fill every empty field on each `providers.<id>` entry whose id matches a
/// built-in. No-op when `config.inherit_defaults` is `false`. No-op for
/// providers without a matching built-in (custom providers stay untouched).
///
/// What "empty" means per field:
/// - `api_base` — empty string.
/// - `api_protocol` — empty [`PatternMap`].
/// - `api_key` — empty string, AND the built-in advertises an env var that
///   resolves to a non-empty value in the current process environment.
///
/// Reads `std::env::var` for credentials. Safe to call repeatedly (idempotent).
pub fn apply_builtin_defaults(config: &mut Config) {
    if !config.inherit_defaults {
        return;
    }
    for (id, provider) in config.providers.iter_mut() {
        let Some(builtin) = builtin::find(id) else {
            continue;
        };
        if provider.api_base.is_empty() {
            provider.api_base = builtin.api_base.clone();
        }
        if provider.api_protocol.is_empty() {
            provider.api_protocol = protocol_mapping_to_pattern_map(&builtin.api_protocol);
        }
        if provider.api_key.is_empty()
            && let Some(env_var) = builtin.auth.env_var()
            && let Ok(value) = env::var(env_var)
            && !value.is_empty()
        {
            provider.api_key = value;
        }
    }
}

/// Translate a built-in's [`ProtocolMapping`] into the existing
/// `PatternMap<ApiProtocol>` used by [`bitrouter_sdk::config::ProviderConfig`].
/// `Single(p)` becomes a single `*` → p entry; `PerModel` keys parse via
/// [`Pattern::parse`] (same wildcard rules used by user-written configs).
fn protocol_mapping_to_pattern_map(m: &ProtocolMapping) -> PatternMap<ApiProtocol> {
    let mut map = PatternMap::new();
    match m {
        ProtocolMapping::Single(p) => map.push(Pattern::Wildcard, p.clone()),
        ProtocolMapping::PerModel(items) => {
            for (k, v) in items {
                map.push(Pattern::parse(k), v.clone());
            }
        }
    }
    map
}

#[cfg(test)]
mod tests {
    use super::*;

    use bitrouter_sdk::config::{Config, ProviderConfig};

    /// Mutating `std::env` in tests is sketchy (it's process-global), so we
    /// run env-var cases serially under a `Mutex` and always set/unset in a
    /// guarded block. The unsafety on `set_var`/`remove_var` is the std API's
    /// reminder that the env table is shared mutable state.
    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        let _g = LOCK.get_or_init(|| Mutex::new(())).lock().unwrap();
        let prev = env::var(key).ok();
        // SAFETY: the test process owns its env; the mutex serialises access.
        unsafe {
            if let Some(v) = value {
                env::set_var(key, v);
            } else {
                env::remove_var(key);
            }
        }
        let result = f();
        // SAFETY: same as above; restore previous value.
        unsafe {
            match prev {
                Some(v) => env::set_var(key, v),
                None => env::remove_var(key),
            }
        }
        result
    }

    fn config_with(id: &str, mut p: ProviderConfig) -> Config {
        let mut c = Config::default();
        p.active = true;
        c.providers.insert(id.to_string(), p);
        c
    }

    fn provider_with_base(api_base: &str) -> ProviderConfig {
        ProviderConfig {
            api_base: api_base.to_string(),
            ..Default::default()
        }
    }

    #[test]
    fn fills_empty_api_base_and_protocol() {
        let mut config = config_with("openai", ProviderConfig::default());
        apply_builtin_defaults(&mut config);
        let p = &config.providers["openai"];
        assert_eq!(p.api_base, "https://api.openai.com/v1");
        assert_eq!(p.api_protocol.resolve("gpt-4o"), Some(&ApiProtocol::Openai));
    }

    #[test]
    fn does_not_overwrite_user_overrides() {
        let user = provider_with_base("https://gateway.internal.example/v1");
        let mut config = config_with("openai", user);
        apply_builtin_defaults(&mut config);
        let p = &config.providers["openai"];
        // user-set api_base wins; api_protocol still gets the built-in default
        assert_eq!(p.api_base, "https://gateway.internal.example/v1");
        assert_eq!(p.api_protocol.resolve("gpt-4o"), Some(&ApiProtocol::Openai));
    }

    #[test]
    fn resolves_env_var_when_present() {
        with_env("OPENAI_API_KEY", Some("sk-from-env-xyz"), || {
            let mut config = config_with("openai", ProviderConfig::default());
            apply_builtin_defaults(&mut config);
            assert_eq!(config.providers["openai"].api_key, "sk-from-env-xyz");
        });
    }

    #[test]
    fn leaves_api_key_empty_when_env_unset() {
        with_env("OPENAI_API_KEY", None, || {
            let mut config = config_with("openai", ProviderConfig::default());
            apply_builtin_defaults(&mut config);
            assert!(config.providers["openai"].api_key.is_empty());
        });
    }

    #[test]
    fn no_op_when_inherit_defaults_false() {
        let mut config = config_with("openai", ProviderConfig::default());
        config.inherit_defaults = false;
        apply_builtin_defaults(&mut config);
        let p = &config.providers["openai"];
        assert!(p.api_base.is_empty());
        assert!(p.api_protocol.is_empty());
    }

    #[test]
    fn ignores_unknown_provider_ids() {
        let mut config = config_with("definitely-not-a-builtin", ProviderConfig::default());
        apply_builtin_defaults(&mut config);
        let p = &config.providers["definitely-not-a-builtin"];
        assert!(p.api_base.is_empty());
        assert!(p.api_protocol.is_empty());
    }

    #[test]
    fn anthropic_carries_header_env_var() {
        with_env("ANTHROPIC_API_KEY", Some("sk-ant-test"), || {
            let mut config = config_with("anthropic", ProviderConfig::default());
            apply_builtin_defaults(&mut config);
            let p = &config.providers["anthropic"];
            assert_eq!(p.api_base, "https://api.anthropic.com/v1");
            assert_eq!(p.api_key, "sk-ant-test");
            assert_eq!(
                p.api_protocol.resolve("claude-opus-4-1"),
                Some(&ApiProtocol::Anthropic)
            );
        });
    }
}
