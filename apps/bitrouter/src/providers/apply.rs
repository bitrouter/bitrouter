//! Fill empty `ProviderConfig` fields from the matching built-in entry.
//!
//! Lets a user write the minimum `providers: { bitrouter: {} }` in their
//! `bitrouter.yaml` and get a fully-populated provider — `api_base`,
//! `api_protocol`, and `api_key` (resolved from the env var the built-in
//! entry advertises) are all filled by [`apply_builtin_defaults`].
//!
//! Opt-out: set `inherit_defaults: false` at the top level of the config.

use bitrouter_ai::types::ProtocolList;
use bitrouter_sdk::config::{Config, Pattern, PatternMap, ProviderConfig};

use crate::providers::builtin;
use crate::providers::entry::ProtocolMapping;
use bitrouter_ai::auth::file::snapshot::CredentialStore;

/// Build the in-memory **zero-config** [`Config`] used when the user
/// runs `bro serve` with no `bitrouter.yaml` anywhere on the
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
/// Only the compiled-in `bitrouter` cloud gateway is auto-enabled here (when
/// `BITROUTER_API_KEY` is set). Every other provider comes from the registry
/// and is auto-enabled by the credential-gated registry merge
/// ([`crate::providers::registry::apply::apply_registry`]) at assembly time, so this
/// function does not enumerate them.
pub fn zero_config() -> Config {
    let mut config = Config::default();
    config.server.listen = "127.0.0.1:4356".to_string();
    config.server.skip_auth = true;
    config.inherit_defaults = true;
    for entry in builtin::all() {
        // Only env-var-credentialed compiled-in built-ins (today: the cloud
        // gateway). The public registry merge supplies metadata and explicit
        // model entries from the resolved dist artifacts.
        let Some(env_var) = entry.auth.env_var() else {
            continue;
        };
        // Go through `bitrouter_sdk::config::env_lookup` so a daemon-
        // side override map (installed by the CLI's `bro reload`)
        // takes precedence over `std::env::var`. This is what
        // lets a newly-exported API key flow into the running daemon's
        // auto-enabled provider list without a full restart.
        if bitrouter_sdk::config::env_lookup(env_var)
            .map(|v| !v.is_empty())
            .unwrap_or(false)
        {
            config
                .providers
                .insert(entry.id.clone(), ProviderConfig::default());
        }
    }
    config
}

/// Provider/credential-variable metadata from the caller's supplied catalog,
/// supplemented by the compiled-in cloud gateway. This reads no environment,
/// files or network. OAuth/native providers have no variable and are omitted.
/// The application chooses the catalog used for reload/onboarding hints.
pub fn zero_config_env_var_providers(
    catalog: Option<&bitrouter_ai::catalog::types::RegistryData>,
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = builtin::all()
        .iter()
        .filter_map(|e| e.auth.env_var().map(|v| (e.id.clone(), v.to_string())))
        .collect();
    if let Some(data) = catalog {
        for p in &data.providers {
            if !p.is_public() {
                continue;
            }
            if let Some(var) = p.env_credential_var()
                && !out.iter().any(|(_, v)| v == &var)
            {
                out.push((p.name.clone(), var));
            }
        }
    }
    out
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
        if bitrouter_ai::providers::retired::provider_message(id).is_some() {
            continue;
        }
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
        // Routing-preference class for built-ins (gateways, the hosted cloud).
        // Registry providers are classed by the registry merge instead; a
        // user-set class wins over both.
        if provider.class.is_none() {
            provider.class = builtin.class;
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
        if id != "bitrouter" && provider.api_key.is_empty() && builtin.auth.env_var().is_some() {
            provider.active = false;
        }
    }
}

/// Re-activate providers that hold a credential in the OAuth credential `store`
/// even though they carry no inline / env-var api key.
///
/// Subscription (OAuth) and "use your Claude Code session" logins persist their
/// credential in the store (`oauth-tokens.json`), not in the config — so
/// The registry merge marks a provider whose only catalog auth is an
/// env-var key (e.g. `anthropic` with `ANTHROPIC_API_KEY`) inactive and drops it
/// from the routing table. This pass restores it: a provider with at least one
/// stored credential is usable, because the matching `AuthApplier` resolves that
/// credential (the live Claude Code session, a stored OAuth token, or a pasted
/// key) at request time. Idempotent; already-active providers are untouched, and
/// providers with no stored credential stay as the configuration merge left
/// them.
pub fn activate_stored_credential_providers(config: &mut Config, store: &CredentialStore) {
    for (id, provider) in config.providers.iter_mut() {
        if bitrouter_ai::providers::retired::provider_message(id).is_some() {
            continue;
        }
        if !provider.active && !store.labels(id).is_empty() {
            provider.active = true;
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
    use crate::providers::test_env::with_env;
    use anyhow::Context;

    use bitrouter_ai::types::ApiProtocol;
    use bitrouter_sdk::config::{Config, ProviderConfig};

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

    // The only compiled-in built-in is the `bitrouter` cloud gateway; the rest
    // come from the registry and are configured by the merge (tested in
    // `registry::apply`). These tests cover `apply_builtin_defaults` /
    // `zero_config` against that one built-in.

    #[test]
    fn fills_empty_api_base_and_protocol() -> anyhow::Result<()> {
        let mut config = config_with("bitrouter", ProviderConfig::default());
        apply_builtin_defaults(&mut config);
        let p = &config.providers["bitrouter"];
        assert_eq!(p.api_base, "https://api.bitrouter.ai/v1");
        assert_eq!(
            p.api_protocol.resolve("gpt-4o"),
            Some(&ProtocolList(vec![ApiProtocol::ChatCompletions]))
        );
        Ok(())
    }

    #[test]
    fn does_not_overwrite_user_overrides() -> anyhow::Result<()> {
        let user = provider_with_base("https://gateway.internal.example/v1");
        let mut config = config_with("bitrouter", user);
        apply_builtin_defaults(&mut config);
        let p = &config.providers["bitrouter"];
        // user-set api_base wins; api_protocol still gets the built-in default
        assert_eq!(p.api_base, "https://gateway.internal.example/v1");
        assert_eq!(
            p.api_protocol.resolve("gpt-4o"),
            Some(&ProtocolList(vec![ApiProtocol::ChatCompletions]))
        );
        Ok(())
    }

    #[test]
    fn resolves_env_var_when_present() -> anyhow::Result<()> {
        with_env("BITROUTER_API_KEY", Some("br-from-env-xyz"), || {
            let mut config = config_with("bitrouter", ProviderConfig::default());
            apply_builtin_defaults(&mut config);
            assert_eq!(config.providers["bitrouter"].api_key, "br-from-env-xyz");
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn leaves_api_key_empty_when_env_unset() -> anyhow::Result<()> {
        with_env("BITROUTER_API_KEY", None, || {
            let mut config = config_with("bitrouter", ProviderConfig::default());
            apply_builtin_defaults(&mut config);
            assert!(config.providers["bitrouter"].api_key.is_empty());
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn no_op_when_inherit_defaults_false() -> anyhow::Result<()> {
        let mut config = config_with("bitrouter", ProviderConfig::default());
        config.inherit_defaults = false;
        apply_builtin_defaults(&mut config);
        let p = &config.providers["bitrouter"];
        assert!(p.api_base.is_empty());
        assert!(p.api_protocol.is_empty());
        Ok(())
    }

    #[test]
    fn ignores_unknown_provider_ids() -> anyhow::Result<()> {
        // openai is no longer compiled in — it is a registry-merge provider, so
        // `apply_builtin_defaults` (which only knows the cloud gateway) leaves
        // it untouched.
        let mut config = config_with("openai", ProviderConfig::default());
        apply_builtin_defaults(&mut config);
        let p = &config.providers["openai"];
        assert!(p.api_base.is_empty());
        assert!(p.api_protocol.is_empty());
        Ok(())
    }

    #[test]
    fn keeps_cloud_provider_active_when_env_key_missing() -> anyhow::Result<()> {
        // BitRouter Cloud can authenticate with the OAuth credential store via
        // its AuthApplier, so absence of BITROUTER_API_KEY must not disable a
        // provider that is already configured.
        with_env("BITROUTER_API_KEY", None, || {
            let mut config = config_with("bitrouter", ProviderConfig::default());
            apply_builtin_defaults(&mut config);
            assert!(config.providers["bitrouter"].active);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn keeps_provider_active_when_user_supplied_key() -> anyhow::Result<()> {
        // A user who hard-codes `api_key` in YAML should stay active
        // regardless of env state.
        with_env("BITROUTER_API_KEY", None, || {
            let p = ProviderConfig {
                api_key: "br-hardcoded".to_string(),
                ..ProviderConfig::default()
            };
            let mut config = config_with("bitrouter", p);
            apply_builtin_defaults(&mut config);
            assert!(config.providers["bitrouter"].active);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn stored_credential_reactivates_keyless_provider() -> anyhow::Result<()> {
        use bitrouter_ai::auth::credentials::Credential;
        use bitrouter_ai::auth::file::snapshot::CredentialStore;
        use bitrouter_ai::auth::store::DEFAULT_ACCOUNT;
        let dir = std::env::temp_dir().join(format!("br-activate-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).context("required fixture value")?;
        let mut store = CredentialStore::load(dir.join("oauth-tokens.json"))
            .context("required fixture value")?;
        // A "use your Claude Code session" login persists this marker.
        store
            .set("anthropic", DEFAULT_ACCOUNT, Credential::ClaudeCodeCli)
            .context("required fixture value")?;

        let mut config = Config::default();
        config.providers.insert(
            "anthropic".to_string(),
            ProviderConfig {
                active: false, // as the merge would leave a keyless Bearer provider
                ..ProviderConfig::default()
            },
        );
        config.providers.insert(
            "openai".to_string(),
            ProviderConfig {
                active: false,
                ..ProviderConfig::default()
            },
        );

        activate_stored_credential_providers(&mut config, &store);
        assert!(
            config.providers["anthropic"].active,
            "a stored credential must re-activate the provider for routing"
        );
        assert!(
            !config.providers["openai"].active,
            "a provider with no stored credential stays inactive"
        );
        Ok(())
    }

    #[test]
    fn zero_config_skips_the_cloud_gateway_without_its_env_var() -> anyhow::Result<()> {
        with_env("BITROUTER_API_KEY", None, || {
            let cfg = zero_config();
            assert!(cfg.server.skip_auth);
            assert!(cfg.inherit_defaults);
            assert_eq!(cfg.server.listen, "127.0.0.1:4356");
            assert!(
                !cfg.providers.contains_key("bitrouter"),
                "the cloud gateway must not be auto-enabled without its key"
            );
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn zero_config_auto_enables_the_cloud_gateway_with_its_env_var() -> anyhow::Result<()> {
        with_env("BITROUTER_API_KEY", Some("br-from-env"), || {
            let mut cfg = zero_config();
            assert!(cfg.providers.contains_key("bitrouter"));
            apply_builtin_defaults(&mut cfg);
            let p = &cfg.providers["bitrouter"];
            assert_eq!(p.api_key, "br-from-env");
            assert_eq!(p.api_base, "https://api.bitrouter.ai/v1");
            assert!(p.active);
            Ok(())
        })?;
        Ok(())
    }

    #[test]
    fn stored_retired_credentials_do_not_reactivate_providers() -> anyhow::Result<()> {
        use bitrouter_ai::auth::credentials::Credential;
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("credentials.json");
        let mut store = CredentialStore::load(&path)?;
        let mut config = Config::default();
        for id in ["google-ai", "vertex"] {
            store.set(
                id,
                "saved",
                Credential::ApiKey {
                    value: "fixture-private".into(),
                },
            )?;
            config.providers.insert(
                id.into(),
                bitrouter_sdk::config::ProviderConfig {
                    active: false,
                    ..Default::default()
                },
            );
        }
        let original = std::fs::read(&path)?;
        activate_stored_credential_providers(&mut config, &store);
        assert!(config.providers.values().all(|provider| !provider.active));
        assert_eq!(std::fs::read(&path)?, original);
        Ok(())
    }
}
