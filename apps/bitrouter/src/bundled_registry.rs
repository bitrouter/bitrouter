//! Provider defaults shipped with the maintained ACP harnesses. Generated from
//! the same registry artifacts as the binary, so fresh installs and a stale
//! public registry can still resolve their subscription login and models.

use anyhow::Result;
use bitrouter_providers::registry::types::RegistryData;
use bitrouter_sdk::config::RegistryConfig;

/// OAuth providers have no credential env var, so the generic registry merge
/// cannot auto-add them. A saved CLI login supplies the activation signal.
pub(crate) fn enable_logged_in(config: &mut bitrouter_sdk::config::Config) {
    if let Ok(store) = bitrouter_providers::oauth::credential_store::CredentialStore::default_path()
    {
        enable_with_store(config, &store);
    }
}

fn enable_with_store(
    config: &mut bitrouter_sdk::config::Config,
    store: &bitrouter_providers::oauth::credential_store::CredentialStore,
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
        "/acp_providers.json"
    )))?)
}

pub(crate) fn provider(id: &str) -> Result<Option<bitrouter_providers::ProviderEntry>> {
    if !matches!(id, "openai-codex" | "claude-code") {
        return Ok(None);
    }
    bundled()?
        .providers
        .iter()
        .find(|provider| provider.name == id)
        .map(bitrouter_providers::builtin::entry_from_registry)
        .transpose()
        .map_err(Into::into)
}

pub(crate) async fn load(registry: &RegistryConfig) -> Option<RegistryData> {
    let remote = bitrouter_providers::registry::apply::load_or_cached(registry).await;
    match supplement(registry, remote.clone()) {
        Ok(data) => data,
        Err(error) => {
            tracing::warn!(%error, "could not load bundled ACP provider defaults");
            remote
        }
    }
}

fn supplement(
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
        if !data.canonical.iter().any(|m| m.id == model.id) {
            data.canonical.push(model);
        }
    }
    Ok(Some(data))
}

#[cfg(test)]
mod tests {
    use super::*;

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
        use bitrouter_providers::oauth::credential_store::{Credential, CredentialStore};
        let dir = tempfile::tempdir()?;
        let mut store = CredentialStore::load(dir.path().join("credentials.json"))?;
        let mut config = bitrouter_sdk::config::Config::default();
        enable_with_store(&mut config, &store);
        assert!(!config.providers.contains_key("openai-codex"));
        store.set("openai-codex", "default", Credential::api_key("test-only"))?;
        enable_with_store(&mut config, &store);
        bitrouter_providers::registry::apply::apply_registry(&mut config, &bundled()?);
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
