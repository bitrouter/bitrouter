//! `PolicyStore` — policies loaded from files. This plugin owns **no database
//! table**; policies are pure config.
//!
//! The store is *reloadable*: when built via [`PolicyStore::load_dir`] it
//! remembers the source directory and [`PolicyStore::reload`] re-scans it. The
//! `PolicyHook` reads via a read lock so reload is safe under concurrent
//! requests — reload must not affect in-flight requests.

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

/// Policies read and validated from disk but not yet installed in the live
/// store. The reload coordinator prepares this before it changes any other
/// runtime participant.
pub(crate) struct PreparedPolicyStore {
    policies: HashMap<String, Policy>,
}

/// Safe inspection result for the configured access-policy directory.
pub(crate) enum PolicySourceState {
    NotConfigured,
    InSync,
    Changed,
    Missing,
    Invalid,
    Unavailable,
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
        let Some(prepared) = self.prepare_reload().await? else {
            return Ok(());
        };
        self.commit_prepared(prepared)
    }

    /// Read a complete replacement policy set without altering the live store.
    /// `None` means this in-memory store has no source directory and therefore
    /// has no reload work.
    pub(crate) async fn prepare_reload(&self) -> Result<Option<PreparedPolicyStore>> {
        let dir = match self.path.read() {
            Ok(path) => path.clone(),
            Err(_) => {
                return Err(BitrouterError::internal(
                    "reading policy reload source failed",
                ));
            }
        };
        let Some(dir) = dir else {
            return Ok(None);
        };
        let policies = scan_policy_dir(&dir).await?;
        Ok(Some(PreparedPolicyStore { policies }))
    }

    /// Install an already prepared replacement policy set.
    pub(crate) fn commit_prepared(&self, prepared: PreparedPolicyStore) -> Result<()> {
        let mut policies = self
            .policies
            .write()
            .map_err(|_| BitrouterError::internal("installing reloaded policies failed"))?;
        *policies = prepared.policies;
        Ok(())
    }

    /// Compare disk and active policy sets without exposing contents or errors.
    pub(crate) async fn source_state(&self) -> PolicySourceState {
        let directory = match self.path.read() {
            Ok(path) => path.clone(),
            Err(_) => return PolicySourceState::Unavailable,
        };
        let Some(directory) = directory else {
            return PolicySourceState::NotConfigured;
        };
        match tokio::fs::metadata(&directory).await {
            Ok(metadata) if metadata.is_dir() => {}
            Ok(_) => return PolicySourceState::Invalid,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return PolicySourceState::Missing;
            }
            Err(_) => return PolicySourceState::Unavailable,
        }
        let saved = match scan_policy_dir(&directory).await {
            Ok(saved) => saved,
            Err(BitrouterError::BadRequest { .. }) => return PolicySourceState::Invalid,
            Err(_) => return PolicySourceState::Unavailable,
        };
        match self.policies.read() {
            Ok(active) if *active == saved => PolicySourceState::InSync,
            Ok(_) => PolicySourceState::Changed,
            Err(_) => PolicySourceState::Unavailable,
        }
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

#[cfg(test)]
mod source_state_tests {
    use super::{PolicySourceState, PolicyStore};

    #[tokio::test]
    async fn source_inspection_distinguishes_invalid_missing_and_changed() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let policies = directory.path().join("policies");
        tokio::fs::create_dir(&policies).await?;
        let file = policies.join("operator.yaml");
        tokio::fs::write(&file, "id: operator\nallowed_models: [first]\n").await?;
        let store = PolicyStore::load_dir(&policies).await?;
        assert!(matches!(
            store.source_state().await,
            PolicySourceState::InSync
        ));

        tokio::fs::write(&file, "id: operator\nallowed_models: [second]\n").await?;
        assert!(matches!(
            store.source_state().await,
            PolicySourceState::Changed
        ));
        tokio::fs::write(&file, "allowed_models: [broken\n").await?;
        assert!(matches!(
            store.source_state().await,
            PolicySourceState::Invalid
        ));
        tokio::fs::remove_file(&file).await?;
        tokio::fs::remove_dir(&policies).await?;
        assert!(matches!(
            store.source_state().await,
            PolicySourceState::Missing
        ));
        assert!(matches!(
            PolicyStore::new().source_state().await,
            PolicySourceState::NotConfigured
        ));
        Ok(())
    }
}
