//! Public provider defaults from the committed registry snapshot. A fresh
//! installation can list providers and resolve login/models without a cache;
//! fetched metadata takes precedence when it is available.

use super::RegistryData;
use crate::config::RegistryConfig;
use anyhow::Result;
use registry_json as serde_json;

/// OAuth providers have no credential env var, so the generic registry merge
/// cannot auto-add them. A saved CLI login supplies the activation signal.
pub(crate) fn enable_logged_in(config: &mut crate::config::Config) {
    if let Ok(store) = crate::provider_credentials::load_default() {
        enable_with_store(config, &store);
    }
}

fn enable_with_store(
    config: &mut crate::config::Config,
    store: &crate::gateway::providers::auth::file::snapshot::CredentialStore,
) {
    if !config.inherit_defaults
        || !config.registry.enabled
        || config.registry.source() != RegistryConfig::default().source()
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
    super::decode::from_slice(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/provider_registry.json"
    )))
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
    let remote = crate::gateway::catalog::cache::load(registry).await;
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
    if !registry.enabled || registry.source() != RegistryConfig::default().source() {
        return Ok(remote);
    }
    if remote.is_some() {
        return Ok(remote);
    }
    let metadata: serde_json::Value = serde_json::from_slice(include_bytes!(concat!(
        env!("OUT_DIR"),
        "/registry_source.json"
    )))?;
    if metadata["source"] != registry.source() {
        return Ok(None);
    }
    Ok(Some(bundled()?))
}

/// Native/OAuth handlers have compiled behavior. Remote metadata cannot alter
/// their public parameter contract or select a handler for another provider.
pub(crate) fn supported_auth(provider: &str, auth: &serde_json::Value) -> bool {
    if !matches!(
        provider,
        "bitrouter" | "openai-codex" | "claude-code" | "github-copilot" | "supergrok"
    ) {
        return false;
    }
    bitrouter_registry::document::Document::parse(
        include_bytes!(concat!(env!("OUT_DIR"), "/provider_registry.json")),
        &bitrouter_registry::document::ReaderPolicy::default(),
    )
    .ok()
    .is_some_and(|doc| {
        doc.providers().iter().any(|p| {
            p["name"] == provider
                && [
                    "kind",
                    "handler",
                    "params",
                    "env",
                    "header",
                    "extra_headers",
                ]
                .iter()
                .all(|key| p["auth"].get(key) == auth.get(key))
        })
    })
}

#[cfg(test)]
mod tests {
    use registry_json as serde_json;
    #[test]
    fn handler_contract_ignores_descriptive_extensions_but_not_parameters() -> anyhow::Result<()> {
        let document = super::bundled()?
            .document
            .ok_or_else(|| anyhow::anyhow!("no document"))?;
        let provider = document["data"]["providers"]
            .as_array()
            .and_then(|p| p.iter().find(|p| p["name"] == "openai-codex"))
            .ok_or_else(|| anyhow::anyhow!("no OAuth provider"))?;
        let mut auth = provider["auth"].clone();
        auth["extensions"] = serde_json::json!({"description":"future harmless metadata"});
        assert!(super::supported_auth("openai-codex", &auth));
        auth["extra_headers"] = serde_json::json!({"future-header":"changed"});
        assert!(!super::supported_auth("openai-codex", &auth));
        auth.as_object_mut()
            .ok_or_else(|| anyhow::anyhow!("auth is not object"))?
            .remove("extra_headers");
        auth["params"]["future_parameter"] = serde_json::json!(true);
        assert!(!super::supported_auth("openai-codex", &auth));
        assert!(!super::supported_auth("other-provider", &provider["auth"]));
        Ok(())
    }
}
