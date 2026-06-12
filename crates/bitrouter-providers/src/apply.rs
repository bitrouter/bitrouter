//! Fill empty `ProviderConfig` fields from the matching built-in entry.
//!
//! Lets a user write the minimum `providers: { openai: {} }` in their
//! `bitrouter.yaml` and get a fully-populated provider — `api_base`,
//! `api_protocol`, and `api_key` (resolved from the env var the built-in
//! entry advertises) are all filled by [`apply_builtin_defaults`].
//!
//! Opt-out: set `inherit_defaults: false` at the top level of the config.

use bitrouter_sdk::config::{Config, Pattern, PatternMap, ProviderConfig};
use bitrouter_sdk::language_model::types::ProtocolList;

use crate::builtin;
use crate::entry::ProtocolMapping;

/// Build the in-memory **zero-config** [`Config`] used when the user
/// runs `bitrouter serve` with no `bitrouter.yaml` anywhere on the
/// resolution chain. Every env-var-based built-in provider whose
/// credential is set in the environment lands in `config.providers` as
/// an empty entry — [`apply_builtin_defaults`] then fills it from the
/// catalog at assembly time. Providers without a credential are left
/// out entirely so the routing table starts empty rather than
/// populated with unusable entries.
///
/// Other zero-config defaults:
/// - `server.listen = "127.0.0.1:4356"` — bind localhost only, since
///   `skip_auth = true` would otherwise expose the gateway with no
///   credential check.
/// - `server.skip_auth = true` — local-first; flip in a written
///   config for multi-tenant use.
/// - `inherit_defaults = true` — built-in catalog fills empty fields.
///
/// `github-copilot` is intentionally *not* auto-enabled: it requires a
/// prior `bitrouter login github-copilot` OAuth flow, which the user
/// has to run explicitly.
pub fn zero_config() -> Config {
    let mut config = Config::default();
    config.server.listen = "127.0.0.1:4356".to_string();
    config.server.skip_auth = true;
    config.inherit_defaults = true;
    for entry in builtin::all() {
        // Only consider env-var-credentialed built-ins. OAuth-only
        // providers (github-copilot) need an explicit user action.
        let Some(env_var) = entry.auth.env_var() else {
            continue;
        };
        // Go through `bitrouter_sdk::config::env_lookup` so a daemon-
        // side override map (installed by the CLI's `bitrouter reload
        // --env`) takes precedence over `std::env::var`. This is what
        // lets a newly-exported API key flow into the running daemon's
        // auto-enabled provider list without a full restart.
        if bitrouter_sdk::config::env_lookup(env_var)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            // `auto_discover: true` so the provider's `/models` endpoint
            // populates the routable model list on startup — zero-config
            // users haven't declared any models explicitly.
            config.providers.insert(
                entry.id.clone(),
                ProviderConfig {
                    auto_discover: true,
                    ..ProviderConfig::default()
                },
            );
        }
    }
    config
}

/// The set of env-var-credentialed built-in provider ids — the ones
/// that participate in [`zero_config`]'s auto-enable check. Stable
/// order so callers can render a human-readable hint.
pub fn zero_config_env_var_providers() -> Vec<(&'static str, &'static str)> {
    builtin::all()
        .iter()
        .filter_map(|e| e.auth.env_var().map(|v| (e.id.as_str(), v)))
        .collect()
}

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
        if provider.protocol_endpoints.is_empty() && !builtin.protocol_endpoints.is_empty() {
            provider.protocol_endpoints = builtin
                .protocol_endpoints
                .iter()
                .map(|(k, v)| (k.clone(), v.clone()))
                .collect();
        }
        // A multi-account provider carries its credentials in `accounts`,
        // not the top-level `api_key`. Skip both the env-var fill and the
        // inactive guard for it — it is explicitly account-managed and
        // already credentialed.
        if !provider.accounts.is_empty() {
            continue;
        }
        if provider.api_key.is_empty()
            && let Some(env_var) = builtin.auth.env_var()
            && let Some(value) = bitrouter_sdk::config::env_lookup(env_var)
            && !value.is_empty()
        {
            provider.api_key = value;
        }
        // Bearer / header auth without a key is unusable — mark the
        // provider inactive so it falls out of the routing table
        // instead of producing requests with an empty `Authorization`
        // line that the upstream rejects. This is what powers the
        // zero-config story: an absent env var doesn't break startup,
        // it just narrows the routable surface to providers the user
        // actually has credentials for. `github-copilot` uses OAuth
        // (no `env_var`), so this guard doesn't touch it.
        if provider.api_key.is_empty() && builtin.auth.env_var().is_some() {
            provider.active = false;
        }
    }
}

/// Translate a built-in's [`ProtocolMapping`] into the
/// `PatternMap<ProtocolList>` used by [`bitrouter_sdk::config::ProviderConfig`].
/// `Single(list)` becomes a single `*` → list entry; `PerModel` keys parse via
/// [`Pattern::parse`] (same wildcard rules used by user-written configs).
fn protocol_mapping_to_pattern_map(m: &ProtocolMapping) -> PatternMap<ProtocolList> {
    let mut map = PatternMap::new();
    match m {
        ProtocolMapping::Single(list) => map.push(Pattern::Wildcard, list.clone()),
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
    use std::env;

    use bitrouter_sdk::config::{Config, ProviderConfig};
    use bitrouter_sdk::language_model::types::ApiProtocol;

    /// Mutating `std::env` in tests is sketchy (it's process-global), so we
    /// run env-var cases serially under a `Mutex` and always set/unset in a
    /// guarded block. The unsafety on `set_var`/`remove_var` is the std API's
    /// reminder that the env table is shared mutable state.
    fn env_lock() -> std::sync::MutexGuard<'static, ()> {
        use std::sync::{Mutex, OnceLock};
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(())).lock().unwrap()
    }

    /// Run `f` with `key` set to `value` (or unset if `None`), restoring
    /// the previous value on exit. Serialises with other env-touching
    /// tests via [`env_lock`].
    fn with_env<R>(key: &str, value: Option<&str>, f: impl FnOnce() -> R) -> R {
        with_envs(&[(key, value)], f)
    }

    /// Multi-variable variant of [`with_env`]. Sets every pair, runs
    /// `f`, restores every pair. One lock acquisition covers them all,
    /// so callers can't deadlock by trying to nest env-tweak helpers.
    fn with_envs<R>(pairs: &[(&str, Option<&str>)], f: impl FnOnce() -> R) -> R {
        let _g = env_lock();
        let prev: Vec<(String, Option<String>)> = pairs
            .iter()
            .map(|(k, _)| ((*k).to_string(), env::var(k).ok()))
            .collect();
        // SAFETY: the test process owns its env; the mutex serialises access.
        unsafe {
            for (k, v) in pairs {
                match v {
                    Some(val) => env::set_var(k, val),
                    None => env::remove_var(k),
                }
            }
        }
        let result = f();
        // SAFETY: same as above; restore previous values.
        unsafe {
            for (k, p) in &prev {
                match p {
                    Some(v) => env::set_var(k, v),
                    None => env::remove_var(k),
                }
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
        assert_eq!(
            p.api_protocol.resolve("gpt-4o"),
            Some(&ProtocolList(vec![
                ApiProtocol::ChatCompletions,
                ApiProtocol::Responses
            ]))
        );
    }

    #[test]
    fn does_not_overwrite_user_overrides() {
        let user = provider_with_base("https://gateway.internal.example/v1");
        let mut config = config_with("openai", user);
        apply_builtin_defaults(&mut config);
        let p = &config.providers["openai"];
        // user-set api_base wins; api_protocol still gets the built-in default
        assert_eq!(p.api_base, "https://gateway.internal.example/v1");
        assert_eq!(
            p.api_protocol.resolve("gpt-4o"),
            Some(&ProtocolList(vec![
                ApiProtocol::ChatCompletions,
                ApiProtocol::Responses
            ]))
        );
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
                Some(&ProtocolList(vec![ApiProtocol::Messages]))
            );
        });
    }

    #[test]
    fn marks_provider_inactive_when_env_key_missing() {
        // The zero-config story relies on this: a built-in entry with
        // no usable credential drops out of routing instead of
        // generating broken upstream requests.
        with_env("OPENAI_API_KEY", None, || {
            let mut config = config_with("openai", ProviderConfig::default());
            apply_builtin_defaults(&mut config);
            assert!(!config.providers["openai"].active);
        });
    }

    #[test]
    fn keeps_provider_active_when_user_supplied_key() {
        // A user who hard-codes `api_key` in YAML should stay active
        // regardless of env state.
        with_env("OPENAI_API_KEY", None, || {
            let p = ProviderConfig {
                api_key: "sk-hardcoded".to_string(),
                ..ProviderConfig::default()
            };
            let mut config = config_with("openai", p);
            apply_builtin_defaults(&mut config);
            assert!(config.providers["openai"].active);
        });
    }

    #[test]
    fn zero_config_skips_providers_without_env_vars() {
        // Clear every env-var-credentialed built-in; result must be a
        // Config with no providers at all (still valid — just routes
        // nothing).
        let pairs: Vec<(&str, Option<&str>)> = zero_config_env_var_providers()
            .into_iter()
            .map(|(_, var)| (var, None))
            .collect();
        with_envs(&pairs, || {
            let cfg = zero_config();
            assert!(cfg.server.skip_auth);
            assert!(cfg.inherit_defaults);
            assert_eq!(cfg.server.listen, "127.0.0.1:4356");
            assert!(
                cfg.providers.is_empty(),
                "expected no providers, got: {:?}",
                cfg.providers.keys().collect::<Vec<_>>()
            );
        });
    }

    #[test]
    fn zero_config_auto_enables_providers_with_env_vars() {
        // Wipe everyone, then set OPENAI_API_KEY.
        let mut pairs: Vec<(&str, Option<&str>)> = zero_config_env_var_providers()
            .into_iter()
            .map(|(_, var)| (var, None))
            .collect();
        pairs.push(("OPENAI_API_KEY", Some("sk-from-env")));
        with_envs(&pairs, || {
            let mut cfg = zero_config();
            assert!(cfg.providers.contains_key("openai"));
            assert!(!cfg.providers.contains_key("anthropic"));
            // After `apply_builtin_defaults` the inserted entry has its
            // catalog defaults filled in.
            apply_builtin_defaults(&mut cfg);
            let p = &cfg.providers["openai"];
            assert_eq!(p.api_key, "sk-from-env");
            assert_eq!(p.api_base, "https://api.openai.com/v1");
            assert!(p.active);
        });
    }
}
