//! `PolicyStore` — policies loaded from files. This plugin owns **no database
//! table**; policies are pure config (003 §4 / 002 §4.4).

use std::collections::HashMap;
use std::path::Path;

use bitrouter_sdk::{BitrouterError, Result};

use crate::policy::{EffectivePolicy, Policy};

/// An in-memory set of named policies.
#[derive(Debug, Clone, Default)]
pub struct PolicyStore {
    policies: HashMap<String, Policy>,
}

impl PolicyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a store from a list of policies.
    pub fn from_policies(policies: impl IntoIterator<Item = Policy>) -> Self {
        let mut store = Self::new();
        for p in policies {
            store.policies.insert(p.id.clone(), p);
        }
        store
    }

    /// Load every `*.yaml` / `*.yml` file in `dir` as one policy.
    pub async fn load_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref();
        let mut store = Self::new();
        let mut entries = tokio::fs::read_dir(dir).await.map_err(|e| {
            BitrouterError::internal(format!("reading policy dir {}: {e}", dir.display()))
        })?;
        while let Some(entry) = entries
            .next_entry()
            .await
            .map_err(|e| BitrouterError::internal(format!("scanning policy dir: {e}")))?
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
            let raw = tokio::fs::read_to_string(&path).await.map_err(|e| {
                BitrouterError::internal(format!("reading {}: {e}", path.display()))
            })?;
            let policy: Policy = serde_saphyr::from_str(&raw).map_err(|e| {
                BitrouterError::bad_request(format!("invalid policy {}: {e}", path.display()))
            })?;
            store.policies.insert(policy.id.clone(), policy);
        }
        Ok(store)
    }

    /// Look up a policy by id.
    pub fn get(&self, id: &str) -> Option<&Policy> {
        self.policies.get(id)
    }

    /// The combined effect of the named policies. Unknown ids are skipped (a
    /// missing policy contributes no constraints — the combination is
    /// permissive by default; see [`EffectivePolicy::combine`]).
    pub fn effective_for(&self, ids: &[&str]) -> EffectivePolicy {
        EffectivePolicy::combine(ids.iter().filter_map(|id| self.policies.get(*id)))
    }

    /// Number of loaded policies.
    pub fn len(&self) -> usize {
        self.policies.len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.policies.is_empty()
    }
}
