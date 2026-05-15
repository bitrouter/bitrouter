//! `PolicyStore` — policies loaded from files. This plugin owns **no database
//! table**; policies are pure config (003 §4 / 002 §4.4).
//!
//! The store is *reloadable*: when built via [`PolicyStore::load_dir`] it
//! remembers the source directory and [`PolicyStore::reload`] re-scans it. The
//! `PolicyHook` reads via a read lock so reload is safe under concurrent
//! requests (008 §3.6 — reload must not affect in-flight requests).

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use bitrouter_sdk::{BitrouterError, Result};

use crate::policy::{EffectivePolicy, Policy};

/// An in-memory, reloadable set of named policies.
#[derive(Debug, Default)]
pub struct PolicyStore {
    policies: RwLock<HashMap<String, Policy>>,
    /// Source directory; set by [`PolicyStore::load_dir`] so [`Self::reload`]
    /// can re-scan. `None` means the store was built in memory (tests / API),
    /// and reload is a no-op.
    path: RwLock<Option<PathBuf>>,
}

impl PolicyStore {
    /// An empty store.
    pub fn new() -> Self {
        Self::default()
    }

    /// Build a store from a list of policies.
    pub fn from_policies(policies: impl IntoIterator<Item = Policy>) -> Self {
        let store = Self::new();
        {
            let mut map = store.policies.write().expect("policy lock poisoned");
            for p in policies {
                map.insert(p.id.clone(), p);
            }
        }
        store
    }

    /// Load every `*.yaml` / `*.yml` file in `dir` as one policy. Remembers
    /// `dir` so [`Self::reload`] can re-read it later.
    pub async fn load_dir(dir: impl AsRef<Path>) -> Result<Self> {
        let dir = dir.as_ref().to_path_buf();
        let fresh = scan_policy_dir(&dir).await?;
        let store = Self::new();
        *store.policies.write().expect("policy lock poisoned") = fresh;
        *store.path.write().expect("policy lock poisoned") = Some(dir);
        Ok(store)
    }

    /// Re-scan the source directory and atomically swap the in-memory set. A
    /// no-op for stores not built from a directory. The new set REPLACES the
    /// old set (a deleted yaml file → that policy is gone).
    pub async fn reload(&self) -> Result<()> {
        let dir = self.path.read().expect("policy lock poisoned").clone();
        let Some(dir) = dir else {
            return Ok(());
        };
        let fresh = scan_policy_dir(&dir).await?;
        *self.policies.write().expect("policy lock poisoned") = fresh;
        Ok(())
    }

    /// Look up a policy by id, applying `f` while the lock is held.
    pub fn with_policy<R>(&self, id: &str, f: impl FnOnce(Option<&Policy>) -> R) -> R {
        let map = self.policies.read().expect("policy lock poisoned");
        f(map.get(id))
    }

    /// The combined effect of the named policies. Unknown ids are skipped (a
    /// missing policy contributes no constraints — the combination is
    /// permissive by default; see [`EffectivePolicy::combine`]).
    pub fn effective_for(&self, ids: &[&str]) -> EffectivePolicy {
        let map = self.policies.read().expect("policy lock poisoned");
        EffectivePolicy::combine(ids.iter().filter_map(|id| map.get(*id)))
    }

    /// Number of loaded policies.
    pub fn len(&self) -> usize {
        self.policies.read().expect("policy lock poisoned").len()
    }

    /// Whether the store is empty.
    pub fn is_empty(&self) -> bool {
        self.policies
            .read()
            .expect("policy lock poisoned")
            .is_empty()
    }
}

async fn scan_policy_dir(dir: &Path) -> Result<HashMap<String, Policy>> {
    let mut out: HashMap<String, Policy> = HashMap::new();
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
        let raw = tokio::fs::read_to_string(&path)
            .await
            .map_err(|e| BitrouterError::internal(format!("reading {}: {e}", path.display())))?;
        let policy: Policy = serde_saphyr::from_str(&raw).map_err(|e| {
            BitrouterError::bad_request(format!("invalid policy {}: {e}", path.display()))
        })?;
        // Operators expect "filename == id" so they can find a policy by its
        // file. Warn (don't fail) when the body's `id` differs — silently
        // shadowing on duplicate id used to mask typos.
        let stem = path.file_stem().and_then(|s| s.to_str()).unwrap_or("");
        if !stem.is_empty() && stem != policy.id {
            tracing::warn!(
                file = %path.display(),
                id = %policy.id,
                filename = %stem,
                "policy filename does not match id: id wins, but operators usually expect them aligned"
            );
        }
        if let Some(prev) = out.insert(policy.id.clone(), policy) {
            tracing::warn!(
                id = %prev.id,
                "duplicate policy id encountered while scanning dir — the later file wins"
            );
        }
    }
    Ok(out)
}
