//! Public provider defaults from the committed registry snapshot. A fresh
//! installation can list providers and resolve login/models without a cache;
//! fetched metadata takes precedence when it is available.

use anyhow::Result;
use bitrouter_ai::catalog::types::RegistryData;
use bitrouter_sdk::config::RegistryConfig;

/// OAuth providers have no credential env var, so the generic registry merge
/// cannot auto-add them. A saved CLI login supplies the activation signal.
pub(crate) fn enable_logged_in(config: &mut bitrouter_sdk::config::Config) {
    if let Ok(store) = crate::provider_credentials::load_default() {
        enable_with_store(config, &store);
    }
}

fn enable_with_store(
    config: &mut bitrouter_sdk::config::Config,
    store: &bitrouter_ai::auth::file::snapshot::CredentialStore,
) {
    if !config.inherit_defaults
        || !config.registry.enabled
        || config.registry.url != RegistryConfig::default().url
    {
        return;
    }
    for id in ["openai-codex", "claude-code"] {
        if !store.labels(id).is_empty() {
            config.providers.entry(id.to_string()).or_default();
        }
    }
}

fn bundled() -> Result<RegistryData> {
    Ok(serde_json::from_str(include_str!(concat!(
        env!("OUT_DIR"),
        "/provider_registry.json"
    )))?)
}

pub(crate) fn provider(id: &str) -> Result<Option<crate::providers::entry::ProviderEntry>> {
    if !matches!(id, "openai-codex" | "claude-code") {
        return Ok(None);
    }
    bundled()?
        .providers
        .iter()
        .find(|provider| provider.name == id)
        .map(crate::providers::builtin::entry_from_registry)
        .transpose()
        .map_err(Into::into)
}

pub(crate) async fn load(registry: &RegistryConfig) -> Option<RegistryData> {
    let remote = crate::catalog::load(registry).await;
    match supplement(registry, remote.clone()) {
        Ok(data) => data,
        Err(error) => {
            tracing::warn!(%error, "could not load bundled provider registry");
            remote
        }
    }
}

pub(crate) fn supplement(
    registry: &RegistryConfig,
    remote: Option<RegistryData>,
) -> Result<Option<RegistryData>> {
    // An operator's custom registry or explicit disable remains authoritative.
    if !registry.enabled || registry.url != RegistryConfig::default().url {
        return Ok(remote);
    }
    let defaults = bundled()?;
    let Some(mut data) = remote else {
        return Ok(Some(defaults));
    };
    for provider in defaults.providers {
        if let Some(published) = data.providers.iter_mut().find(|p| p.name == provider.name) {
            for model in provider.models {
                if !published.models.iter().any(|m| m.id == model.id) {
                    published.models.push(model);
                }
            }
        } else {
            data.providers.push(provider);
        }
    }
    for model in defaults.canonical {
        if let Some(published) = data.canonical.iter_mut().find(|m| m.id == model.id) {
            // Older caches retained only identity. Restore missing capacities
            // offline while keeping every explicit value from the remote catalog.
            published.max_input_tokens = published.max_input_tokens.or(model.max_input_tokens);
            published.max_output_tokens = published.max_output_tokens.or(model.max_output_tokens);
            published.context_window = published.context_window.or(model.context_window);
        } else {
            data.canonical.push(model);
        }
    }
    Ok(Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;

    #[test]
    fn offline_catalog_preserves_all_committed_providers() -> Result<()> {
        let committed: serde_json::Value =
            serde_json::from_str(include_str!("../../../dist/registry/providers.json"))?;
        let data = supplement(&RegistryConfig::default(), None)?.context("no offline catalog")?;
        let actual: std::collections::BTreeSet<_> = data
            .providers
            .iter()
            .map(|provider| provider.name.as_str())
            .collect();
        let expected: std::collections::BTreeSet<_> = committed["data"]
            .as_array()
            .context("no provider array")?
            .iter()
            .filter_map(|provider| provider["name"].as_str())
            .collect();
        assert_eq!(actual, expected);
        for provider in data
            .providers
            .iter()
            .filter(|provider| provider.is_active() && provider.is_public())
        {
            crate::providers::builtin::entry_from_registry(provider)?;
        }
        Ok(())
    }

    #[test]
    fn subscription_login_defaults_are_available_without_a_cache() -> Result<()> {
        let data = supplement(&RegistryConfig::default(), None)?
            .ok_or_else(|| anyhow::anyhow!("no defaults"))?;
        for id in ["openai-codex", "claude-code"] {
            assert!(provider(id)?.is_some());
            assert!(
                data.providers
                    .iter()
                    .any(|p| p.name == id && !p.models.is_empty())
            );
        }
        assert!(provider("custom")?.is_none());
        Ok(())
    }

    #[test]
    fn stored_login_activates_subscription_without_a_provider_config_entry() -> Result<()> {
        use bitrouter_ai::auth::credentials::Credential;
        use bitrouter_ai::auth::file::snapshot::CredentialStore;
        let dir = tempfile::tempdir()?;
        let mut store = CredentialStore::load(dir.path().join("credentials.json"))?;
        let mut config = bitrouter_sdk::config::Config::default();
        enable_with_store(&mut config, &store);
        assert!(!config.providers.contains_key("openai-codex"));
        store.set("openai-codex", "default", Credential::api_key("test-only"))?;
        enable_with_store(&mut config, &store);
        crate::providers::registry::apply::apply_registry(&mut config, &bundled()?);
        let provider = config
            .providers
            .get("openai-codex")
            .ok_or_else(|| anyhow::anyhow!("provider not activated"))?;
        assert!(provider.active);
        assert!(!provider.api_base.is_empty());
        assert!(!provider.models.is_empty());
        assert!(!config.providers.contains_key("claude-code"));
        Ok(())
    }

    #[test]
    fn supplementation_keeps_remote_metadata_and_is_idempotent() -> Result<()> {
        let mut remote = bundled()?;
        let first = remote
            .providers
            .first_mut()
            .ok_or_else(|| anyhow::anyhow!("empty defaults"))?;
        let id = first.name.clone();
        first.api_base = Some("https://example.invalid/new-endpoint".into());
        first.models.clear();
        let once = supplement(&RegistryConfig::default(), Some(remote))?
            .ok_or_else(|| anyhow::anyhow!("no registry"))?;
        let provider = once
            .providers
            .iter()
            .find(|p| p.name == id)
            .ok_or_else(|| anyhow::anyhow!("lost provider"))?;
        assert_eq!(
            provider.api_base.as_deref(),
            Some("https://example.invalid/new-endpoint")
        );
        assert!(!provider.models.is_empty());
        let twice = supplement(&RegistryConfig::default(), Some(once.clone()))?;
        assert_eq!(
            serde_json::to_value(twice)?,
            serde_json::to_value(Some(once))?
        );
        Ok(())
    }

    #[test]
    fn old_identity_only_cache_recovers_bundled_capacities() -> Result<()> {
        let defaults = bundled()?;
        let metadata = defaults
            .canonical
            .iter()
            .find(|model| model.max_output_tokens.is_some())
            .context("no output capacity")?;
        let mut old = defaults.clone();
        for model in &mut old.canonical {
            model.max_input_tokens = None;
            model.max_output_tokens = None;
            model.context_window = None;
        }
        let repaired = supplement(&RegistryConfig::default(), Some(old))?.context("no registry")?;
        let model = repaired
            .canonical
            .iter()
            .find(|model| model.id == metadata.id)
            .context("lost model")?;
        assert_eq!(model.max_output_tokens, metadata.max_output_tokens);
        let mut remote = repaired;
        let model = remote
            .canonical
            .iter_mut()
            .find(|model| model.id == metadata.id)
            .context("lost model")?;
        model.max_output_tokens = Some(12345);
        let retained =
            supplement(&RegistryConfig::default(), Some(remote))?.context("no registry")?;
        assert_eq!(
            retained
                .canonical
                .iter()
                .find(|model| model.id == metadata.id)
                .context("lost model")?
                .max_output_tokens,
            Some(12345)
        );
        Ok(())
    }

    #[test]
    fn disabled_and_custom_registries_do_not_receive_defaults() -> Result<()> {
        for registry in [
            RegistryConfig {
                enabled: false,
                ..Default::default()
            },
            RegistryConfig {
                url: "https://example.invalid/private".into(),
                ..Default::default()
            },
        ] {
            assert!(supplement(&registry, None)?.is_none());
        }
        Ok(())
    }
}
