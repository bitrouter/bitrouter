use bitrouter_ai::catalog::Catalog;
use bitrouter_ai::catalog::store::{CatalogSnapshot, CatalogStore, Durability, StoreError};
use bitrouter_ai::catalog::types::{CanonicalModel, RegistryData};
use bitrouter_sdk::config::DEFAULT_REGISTRY_URL;
use std::sync::Arc;

use super::{CachedPayload, DiskCache, TTL};

fn snapshot(id: &str) -> CatalogSnapshot {
    CatalogSnapshot {
        fetched_at: 0,
        data: RegistryData {
            providers: Vec::new(),
            canonical: vec![CanonicalModel { id: id.into() }],
        },
    }
}

#[test]
fn unbound_legacy_cache_is_preserved_without_guessing_its_source() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("registry.json");
    std::fs::write(
        &path,
        serde_json::to_vec(&CachedPayload {
            source: None,
            snapshot: snapshot("old/model"),
        })?,
    )?;
    let original = std::fs::read(&path)?;
    let cache = Arc::new(DiskCache::at(&path));
    let mut public = Catalog::with_store(DEFAULT_REGISTRY_URL, cache.clone());
    public.load()?;
    assert!(public.snapshot().is_none());
    assert!(!public.is_fresh(TTL));
    let mut custom = Catalog::with_store("https://private.invalid/catalog", cache);
    custom.load()?;
    assert!(custom.snapshot().is_none());
    assert_eq!(std::fs::read(path)?, original);
    Ok(())
}

#[test]
fn persistent_snapshot_round_trip_is_bound_to_its_source() -> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let cache = DiskCache::at(dir.path().join("registry.json"));
    assert!(cache.load("https://first.invalid")?.is_none());
    assert_eq!(
        cache.save("https://first.invalid/", &snapshot("first/model"))?,
        Durability::Persistent
    );
    assert_eq!(
        cache
            .load("https://first.invalid")?
            .ok_or_else(|| anyhow::anyhow!("missing persisted snapshot"))?
            .data
            .canonical[0]
            .id,
        "first/model"
    );
    assert!(cache.load("https://second.invalid")?.is_none());
    assert!(cache.load(DEFAULT_REGISTRY_URL)?.is_none());
    Ok(())
}

#[test]
fn corrupt_cache_is_an_explicit_error_and_failed_save_preserves_previous_file() -> anyhow::Result<()>
{
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("registry.json");
    let cache = DiskCache::at(&path);
    std::fs::write(&path, b"not json")?;
    assert!(matches!(cache.load("source"), Err(StoreError::Invalid)));
    cache.save("source", &snapshot("old/model"))?;
    std::fs::create_dir(path.with_extension("json.tmp"))?;
    assert!(matches!(
        cache.save("source", &snapshot("new/model")),
        Err(StoreError::Unavailable)
    ));
    assert_eq!(
        cache
            .load("source")?
            .ok_or_else(|| anyhow::anyhow!("old snapshot lost"))?
            .data
            .canonical[0]
            .id,
        "old/model"
    );
    Ok(())
}

#[test]
fn bundled_credential_variables_are_available_without_any_cache_or_network() -> anyhow::Result<()> {
    let defaults = crate::bundled_registry::supplement(
        &bitrouter_sdk::config::RegistryConfig::default(),
        None,
    )?
    .ok_or_else(|| anyhow::anyhow!("missing baseline"))?;
    let variables = crate::providers::apply::zero_config_env_var_providers(Some(&defaults));
    for provider in defaults
        .providers
        .iter()
        .filter(|provider| provider.is_public())
    {
        if let Some(variable) = provider.env_credential_var() {
            assert!(variables.iter().any(|(_, value)| value == &variable));
        }
    }
    Ok(())
}

#[test]
fn reload_keeps_custom_source_credential_names_without_mixing_routing_catalogs()
-> anyhow::Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("registry.json");
    let cache = DiskCache::at(&path);
    let mut custom = snapshot("custom/model");
    custom
        .data
        .providers
        .push(serde_json::from_value(serde_json::json!({
            "name":"custom-source-provider", "status":"active",
            "auth":{"kind":"bearer", "env":"CUSTOM_CATALOG_KEY"}, "models":[]
        }))?);
    cache.save("https://custom.invalid/catalog", &custom)?;
    let variables = super::credential_variables(Some(DiskCache::at(&path)));
    assert!(
        variables
            .iter()
            .any(|(_, variable)| variable == "CUSTOM_CATALOG_KEY")
    );
    assert!(
        !super::credential_variables(None)
            .iter()
            .any(|(_, variable)| variable == "CUSTOM_CATALOG_KEY")
    );
    let mut public = Catalog::with_store(DEFAULT_REGISTRY_URL, Arc::new(cache));
    public.load()?;
    assert!(public.snapshot().is_none());
    Ok(())
}

#[test]
fn retired_catalog_cache_is_quarantined_without_rewriting_saved_bytes() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("registry.json");
    let bytes = serde_json::to_vec(
        &serde_json::json!({"source":DEFAULT_REGISTRY_URL,"fetched_at":1,"data":{"canonical":[{"id":"org/chat"}],"providers":[
            {"name":"google-ai","status":"active","models":[{"api_protocol":"antigravity"}]},
            {"name":"fixture","status":"active","models":[{"id":"org/chat","provider_model_id":"chat","api_protocol":"openai"}]}
        ]}}),
    )?;
    std::fs::write(&path, &bytes)?;
    let loaded = DiskCache::at(&path)
        .load(DEFAULT_REGISTRY_URL)?
        .ok_or_else(|| anyhow::anyhow!("cache unavailable"))?;
    assert_eq!(loaded.data.providers.len(), 1);
    assert_eq!(loaded.data.providers[0].name, "fixture");
    assert_eq!(std::fs::read(&path)?, bytes);
    Ok(())
}
