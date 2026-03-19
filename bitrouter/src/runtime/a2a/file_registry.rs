//! File-based agent card registry.
//!
//! Stores agent registrations as JSON files in a directory:
//! ```text
//! <agents_dir>/
//! ├── claude-code.json
//! ├── cursor.json
//! └── ...
//! ```

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::RwLock;

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::registry::{AgentCardRegistry, AgentRegistration, validate_name};

/// A file-based [`AgentCardRegistry`] that stores each registration as a
/// JSON file named `<agent_name>.json` in a directory.
pub struct FileAgentCardRegistry {
    dir: PathBuf,
    /// Serializes file operations for thread safety.
    lock: RwLock<()>,
}

impl FileAgentCardRegistry {
    /// Create a new registry backed by the given directory.
    ///
    /// The directory is created if it does not exist.
    pub fn new(dir: &Path) -> Result<Self, A2aError> {
        fs::create_dir_all(dir)
            .map_err(|e| A2aError::Storage(format!("failed to create agents dir: {e}")))?;
        Ok(Self {
            dir: dir.to_path_buf(),
            lock: RwLock::new(()),
        })
    }

    fn agent_path(&self, name: &str) -> PathBuf {
        self.dir.join(format!("{name}.json"))
    }
}

impl AgentCardRegistry for FileAgentCardRegistry {
    fn register(&self, registration: AgentRegistration) -> Result<(), A2aError> {
        validate_name(&registration.card.name)?;
        let _guard = self
            .lock
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        let path = self.agent_path(&registration.card.name);
        if path.exists() {
            return Err(A2aError::AlreadyExists {
                name: registration.card.name.clone(),
            });
        }

        let json = serde_json::to_string_pretty(&registration)
            .map_err(|e| A2aError::Storage(format!("failed to serialize: {e}")))?;
        fs::write(&path, json)
            .map_err(|e| A2aError::Storage(format!("failed to write {}: {e}", path.display())))?;

        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), A2aError> {
        let _guard = self
            .lock
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        let path = self.agent_path(name);
        if !path.exists() {
            return Err(A2aError::NotFound {
                name: name.to_string(),
            });
        }

        fs::remove_file(&path)
            .map_err(|e| A2aError::Storage(format!("failed to remove {}: {e}", path.display())))?;

        Ok(())
    }

    fn get(&self, name: &str) -> Result<Option<AgentRegistration>, A2aError> {
        let _guard = self
            .lock
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        let path = self.agent_path(name);
        if !path.exists() {
            return Ok(None);
        }

        let contents = fs::read_to_string(&path)
            .map_err(|e| A2aError::Storage(format!("failed to read {}: {e}", path.display())))?;
        let reg: AgentRegistration = serde_json::from_str(&contents)
            .map_err(|e| A2aError::Storage(format!("failed to parse {}: {e}", path.display())))?;

        Ok(Some(reg))
    }

    fn list(&self) -> Result<Vec<AgentRegistration>, A2aError> {
        let _guard = self
            .lock
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        if !self.dir.exists() {
            return Ok(Vec::new());
        }

        let entries = fs::read_dir(&self.dir)
            .map_err(|e| A2aError::Storage(format!("failed to read dir: {e}")))?;

        let mut registrations = Vec::new();
        for entry in entries {
            let entry =
                entry.map_err(|e| A2aError::Storage(format!("failed to read entry: {e}")))?;
            let path = entry.path();

            if path.extension().is_some_and(|ext| ext == "json") {
                match fs::read_to_string(&path) {
                    Ok(contents) => match serde_json::from_str::<AgentRegistration>(&contents) {
                        Ok(reg) => registrations.push(reg),
                        Err(e) => {
                            tracing::warn!("skipping invalid agent file {}: {e}", path.display());
                        }
                    },
                    Err(e) => {
                        tracing::warn!("skipping unreadable agent file {}: {e}", path.display());
                    }
                }
            }
        }

        registrations.sort_by(|a, b| a.card.name.cmp(&b.card.name));
        Ok(registrations)
    }

    fn resolve_by_iss(&self, iss: &str) -> Result<Option<String>, A2aError> {
        let registrations = self.list()?;
        for reg in &registrations {
            if reg.iss.as_deref() == Some(iss) {
                return Ok(Some(reg.card.name.clone()));
            }
        }
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_a2a::card::minimal_card;

    fn test_registration(name: &str) -> AgentRegistration {
        AgentRegistration {
            card: minimal_card(name, "test agent", "1.0.0", "http://localhost:8787"),
            iss: None,
        }
    }

    #[test]
    fn register_and_get() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = FileAgentCardRegistry::new(dir.path()).expect("new registry");

        registry
            .register(test_registration("test-agent"))
            .expect("register");

        let reg = registry.get("test-agent").expect("get").expect("found");
        assert_eq!(reg.card.name, "test-agent");
    }

    #[test]
    fn register_duplicate_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = FileAgentCardRegistry::new(dir.path()).expect("new registry");

        registry
            .register(test_registration("test-agent"))
            .expect("register");

        let err = registry
            .register(test_registration("test-agent"))
            .expect_err("duplicate");
        assert!(matches!(err, A2aError::AlreadyExists { .. }));
    }

    #[test]
    fn remove_and_verify() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = FileAgentCardRegistry::new(dir.path()).expect("new registry");

        registry
            .register(test_registration("test-agent"))
            .expect("register");
        registry.remove("test-agent").expect("remove");

        assert!(registry.get("test-agent").expect("get").is_none());
    }

    #[test]
    fn remove_missing_fails() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = FileAgentCardRegistry::new(dir.path()).expect("new registry");

        let err = registry.remove("nonexistent").expect_err("not found");
        assert!(matches!(err, A2aError::NotFound { .. }));
    }

    #[test]
    fn list_sorted() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = FileAgentCardRegistry::new(dir.path()).expect("new registry");

        registry
            .register(test_registration("zeta"))
            .expect("register");
        registry
            .register(test_registration("alpha"))
            .expect("register");
        registry
            .register(test_registration("beta"))
            .expect("register");

        let list = registry.list().expect("list");
        let names: Vec<&str> = list.iter().map(|r| r.card.name.as_str()).collect();
        assert_eq!(names, vec!["alpha", "beta", "zeta"]);
    }

    #[test]
    fn resolve_by_iss_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = FileAgentCardRegistry::new(dir.path()).expect("new registry");

        let mut reg = test_registration("claude-code");
        reg.iss = Some("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpb123".to_string());
        registry.register(reg).expect("register");

        let resolved = registry
            .resolve_by_iss("solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:DRpb123")
            .expect("resolve")
            .expect("found");
        assert_eq!(resolved, "claude-code");
    }

    #[test]
    fn resolve_by_iss_not_found() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = FileAgentCardRegistry::new(dir.path()).expect("new registry");

        registry
            .register(test_registration("test-agent"))
            .expect("register");

        let resolved = registry.resolve_by_iss("unknown:iss").expect("resolve");
        assert!(resolved.is_none());
    }

    #[test]
    fn invalid_name_rejected() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = FileAgentCardRegistry::new(dir.path()).expect("new registry");

        let mut reg = test_registration("test-agent");
        reg.card.name = "Invalid Name".to_string();
        let err = registry.register(reg).expect_err("invalid name");
        assert!(matches!(err, A2aError::InvalidName { .. }));
    }
}
