//! `RegistryRoutingTable` — a `RoutingTable` backed by an external
//! `provider-registry` directory.
//!
//! It shares the **same registry-style provider schema** as
//! [`crate::config::ConfigRoutingTable`] — the only difference is
//! the data source. The registry directory holds one YAML file per provider
//! (`<provider-id>.yaml`, each a [`crate::config::ProviderConfig`]); the
//! routing logic itself is reused via an inner `ConfigRoutingTable`.
//!
//! In cloud this table is paired with a recommender + circuit breaker that
//! keep evolving; v1's `RegistryRoutingTable` is the architecture seat for
//! that, tracking cloud's current shape.

use std::path::PathBuf;
use std::sync::RwLock;

use async_trait::async_trait;

use crate::caller::CallerContext;
use crate::config::routing_table::{list_models_for, resolve_route_chain};
use crate::config::{Config, ProviderConfig};
use crate::error::{BitrouterError, Result};
use crate::language_model::routing::{ModelInfo, RoutingPrefs, RoutingTable};
use crate::language_model::types::RoutingTarget;

/// A `RoutingTable` over an external provider-registry directory.
pub struct RegistryRoutingTable {
    /// The registry directory — one `<provider>.yaml` per provider.
    dir: PathBuf,
    /// The registry-sourced config; rebuilt on `reload()`.
    config: RwLock<Config>,
}

impl RegistryRoutingTable {
    /// Load a registry from `dir`. Each `*.yaml` file is one provider entry,
    /// keyed by its file stem.
    pub async fn load(dir: impl Into<PathBuf>) -> Result<Self> {
        let dir = dir.into();
        let config = Self::scan(&dir).await?;
        Ok(Self {
            dir,
            config: RwLock::new(config),
        })
    }

    /// Scan the registry directory into a `Config` carrying only `providers`.
    async fn scan(dir: &std::path::Path) -> Result<Config> {
        let mut config = Config::default();
        let mut entries = tokio::fs::read_dir(dir).await.map_err(|e| {
            BitrouterError::internal(format!("reading registry dir {}: {e}", dir.display()))
        })?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| BitrouterError::internal(format!("scanning registry dir: {e}")))?
        {
            let path = entry.path();
            let is_yaml = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|e| e == "yaml" || e == "yml")
                .unwrap_or(false);
            if !is_yaml {
                continue;
            }
            let Some(provider_id) = path.file_stem().and_then(|s| s.to_str()) else {
                continue;
            };
            let raw = tokio::fs::read_to_string(&path).await.map_err(|e| {
                BitrouterError::internal(format!("reading {}: {e}", path.display()))
            })?;
            let substituted = crate::config::substitute_env(&raw)?;
            let provider: ProviderConfig = serde_saphyr::from_str(&substituted).map_err(|e| {
                BitrouterError::bad_request(format!(
                    "invalid registry provider {}: {e}",
                    path.display()
                ))
            })?;
            config.providers.insert(provider_id.to_string(), provider);
        }
        Ok(config)
    }
}

#[async_trait]
impl RoutingTable for RegistryRoutingTable {
    async fn route_chain(
        &self,
        model: &str,
        prefs: &RoutingPrefs,
        _caller: &CallerContext,
    ) -> Result<Vec<RoutingTarget>> {
        // Resolution is synchronous — the read guard is not held across an await.
        let config = self.config.read().expect("registry lock poisoned");
        resolve_route_chain(&config, model, prefs)
    }

    fn list_models(&self) -> Vec<ModelInfo> {
        let config = self.config.read().expect("registry lock poisoned");
        list_models_for(&config)
    }

    fn model_info(&self, model: &str) -> Option<ModelInfo> {
        self.list_models().into_iter().find(|m| m.id == model)
    }

    async fn reload(&self) -> Result<()> {
        let config = Self::scan(&self.dir).await?;
        *self.config.write().expect("registry lock poisoned") = config;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::routing::RoutingPrefs;

    #[tokio::test]
    async fn loads_providers_from_a_registry_dir() {
        let dir = std::env::temp_dir().join(format!(
            "brregistry-{}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        tokio::fs::create_dir_all(&dir).await.unwrap();
        tokio::fs::write(
            dir.join("openai.yaml"),
            "api_base: https://api.openai.com/v1\napi_key: k1\nmodels: [{ id: gpt-5 }]\n",
        )
        .await
        .unwrap();
        tokio::fs::write(
            dir.join("anthropic.yaml"),
            "api_base: https://api.anthropic.com/v1\napi_key: k2\nmodels: [{ id: gpt-5 }]\n",
        )
        .await
        .unwrap();

        let table = RegistryRoutingTable::load(&dir).await.unwrap();
        // both registry providers declare gpt-5 → a 2-hop cascade chain,
        // resolved by the same shared logic as ConfigRoutingTable
        let chain = table
            .route_chain("gpt-5", &RoutingPrefs::default(), &CallerContext::local())
            .await
            .unwrap();
        assert_eq!(chain.len(), 2);
        // protocol is inferred from the api-base host
        let anthropic = chain
            .iter()
            .find(|t| t.provider_name == "anthropic")
            .unwrap();
        assert_eq!(
            anthropic.api_protocol,
            crate::language_model::types::ApiProtocol::Messages
        );
        let _ = tokio::fs::remove_dir_all(&dir).await;
    }
}
