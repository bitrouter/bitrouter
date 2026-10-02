//! Named remote-control targets shared by the CLI and operations dashboard.
//!
//! Contexts store only an endpoint and the *name* of an environment variable.
//! Bearer values never enter this file.

use std::collections::BTreeMap;
use std::io::Write;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result};
use bitrouter_sdk::error::BitrouterError;
use serde::{Deserialize, Serialize};

use crate::output::CliReport;
use crate::output::human::{Human, Table};

const STORE_VERSION: u32 = 1;
const STORE_FILENAME: &str = "contexts.toml";

/// One named remote BitRouter target.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RemoteContext {
    pub endpoint: String,
    pub token_env: String,
}

impl RemoteContext {
    pub fn new(endpoint: &str, token_env: &str) -> Result<Self> {
        if !valid_env_name(token_env) {
            return Err(BitrouterError::bad_request(format!(
                "token environment variable must match [A-Za-z_][A-Za-z0-9_]*, got '{token_env}'"
            ))
            .into());
        }
        Ok(Self {
            endpoint: crate::remote_control::normalize_endpoint(endpoint)
                .map_err(BitrouterError::bad_request)?
                .to_string(),
            token_env: token_env.to_string(),
        })
    }

    pub fn client(&self) -> Result<crate::remote_control::HttpControlClient> {
        crate::remote_control::HttpControlClient::new(&self.endpoint, &self.token_env)
    }
}

/// Versioned on-disk context store.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct ContextStore {
    version: u32,
    contexts: BTreeMap<String, RemoteContext>,
}

impl Default for ContextStore {
    fn default() -> Self {
        Self {
            version: STORE_VERSION,
            contexts: BTreeMap::new(),
        }
    }
}

impl ContextStore {
    pub fn load() -> Result<Self> {
        Self::load_from(&store_path()?)
    }

    pub fn load_from(path: &Path) -> Result<Self> {
        let text = match std::fs::read_to_string(path) {
            Ok(text) => text,
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
                return Ok(Self::default());
            }
            Err(error) => {
                return Err(error)
                    .with_context(|| format!("read context store {}", path.display()));
            }
        };
        let store: Self = toml::from_str(&text)
            .with_context(|| format!("parse context store {}", path.display()))?;
        if store.version != STORE_VERSION {
            return Err(BitrouterError::bad_request(format!(
                "context store {} uses unsupported version {} (client supports {})",
                path.display(),
                store.version,
                STORE_VERSION
            ))
            .into());
        }
        Ok(store)
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&store_path()?)
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let parent = path.parent().ok_or_else(|| {
            anyhow::anyhow!("context store path has no parent: {}", path.display())
        })?;
        std::fs::create_dir_all(parent)
            .with_context(|| format!("create context directory {}", parent.display()))?;
        let text = toml::to_string_pretty(self).context("serialize context store")?;
        let mut temporary = tempfile::NamedTempFile::new_in(parent)
            .with_context(|| format!("create temporary context store in {}", parent.display()))?;
        temporary
            .write_all(text.as_bytes())
            .context("write temporary context store")?;
        temporary
            .as_file()
            .sync_all()
            .context("flush temporary context store")?;
        protect_file(temporary.path())?;
        temporary
            .persist(path)
            .map_err(|error| error.error)
            .with_context(|| format!("replace context store {}", path.display()))?;
        Ok(())
    }

    pub fn add(&mut self, name: &str, context: RemoteContext) -> Result<()> {
        validate_context_name(name)?;
        if self.contexts.contains_key(name) {
            return Err(BitrouterError::bad_request(format!(
                "context '{name}' already exists; remove it before replacing it"
            ))
            .into());
        }
        self.contexts.insert(name.to_string(), context);
        Ok(())
    }

    pub fn remove(&mut self, name: &str) -> Result<RemoteContext> {
        validate_context_name(name)?;
        self.contexts
            .remove(name)
            .ok_or_else(|| BitrouterError::NotFound(format!("context '{name}' does not exist")))
            .map_err(Into::into)
    }

    pub fn get(&self, name: &str) -> Result<&RemoteContext> {
        if name == "local" {
            return Err(BitrouterError::bad_request(
                "'local' is the built-in context, not a stored remote context",
            )
            .into());
        }
        self.contexts
            .get(name)
            .ok_or_else(|| BitrouterError::NotFound(format!("context '{name}' does not exist")))
            .map_err(Into::into)
    }

    pub fn report(&self) -> ContextsReport {
        let contexts = self
            .contexts
            .iter()
            .map(|(name, context)| ContextRow {
                name: name.clone(),
                endpoint: context.endpoint.clone(),
                token_env: context.token_env.clone(),
            })
            .collect();
        ContextsReport { contexts }
    }
}

pub fn resolve(name: &str) -> Result<Option<RemoteContext>> {
    if name == "local" {
        return Ok(None);
    }
    Ok(Some(ContextStore::load()?.get(name)?.clone()))
}

pub fn add(name: &str, endpoint: &str, token_env: &str) -> Result<ContextReport> {
    let context = RemoteContext::new(endpoint, token_env)?;
    let mut store = ContextStore::load()?;
    store.add(name, context.clone())?;
    store.save()?;
    Ok(ContextReport::new("added", name, &context))
}

pub fn remove(name: &str) -> Result<ContextReport> {
    let mut store = ContextStore::load()?;
    let context = store.remove(name)?;
    store.save()?;
    Ok(ContextReport::new("removed", name, &context))
}

pub fn show(name: &str) -> Result<ContextReport> {
    let store = ContextStore::load()?;
    let context = store.get(name)?;
    Ok(ContextReport::new("shown", name, context))
}

pub fn list() -> Result<ContextsReport> {
    Ok(ContextStore::load()?.report())
}

fn store_path() -> Result<PathBuf> {
    if let Some(home) = std::env::var_os("BITROUTER_HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(STORE_FILENAME));
    }
    if let Some(home) = std::env::var_os("HOME").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(".bitrouter").join(STORE_FILENAME));
    }
    #[cfg(windows)]
    if let Some(home) = std::env::var_os("USERPROFILE").filter(|value| !value.is_empty()) {
        return Ok(PathBuf::from(home).join(".bitrouter").join(STORE_FILENAME));
    }
    anyhow::bail!("cannot locate context store; set BITROUTER_HOME or HOME")
}

fn validate_context_name(name: &str) -> Result<()> {
    if name == "local" {
        return Err(BitrouterError::bad_request(
            "'local' is reserved for the built-in local context",
        )
        .into());
    }
    let valid = !name.is_empty()
        && name.len() <= 64
        && name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.'));
    if !valid {
        return Err(BitrouterError::bad_request(format!(
            "context name must be 1-64 ASCII letters, digits, '.', '-', or '_', got '{name}'"
        ))
        .into());
    }
    Ok(())
}

fn valid_env_name(name: &str) -> bool {
    let mut bytes = name.bytes();
    let Some(first) = bytes.next() else {
        return false;
    };
    (first.is_ascii_alphabetic() || first == b'_')
        && bytes.all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
}

#[cfg(unix)]
fn protect_file(path: &Path) -> Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))
        .with_context(|| format!("protect context store {}", path.display()))
}

#[cfg(not(unix))]
fn protect_file(_path: &Path) -> Result<()> {
    Ok(())
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextRow {
    pub name: String,
    pub endpoint: String,
    pub token_env: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextsReport {
    pub contexts: Vec<ContextRow>,
}

impl CliReport for ContextsReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        if self.contexts.is_empty() {
            return human.line("(no remote contexts; local is the default)");
        }
        let mut table = Table::new(["NAME", "ENDPOINT", "TOKEN_ENV"]);
        for context in &self.contexts {
            table.push([
                context.name.clone(),
                context.endpoint.clone(),
                context.token_env.clone(),
            ]);
        }
        human.table(&table)
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct ContextReport {
    pub action: &'static str,
    pub name: String,
    pub endpoint: String,
    pub token_env: String,
}

impl ContextReport {
    fn new(action: &'static str, name: &str, context: &RemoteContext) -> Self {
        Self {
            action,
            name: name.to_string(),
            endpoint: context.endpoint.clone(),
            token_env: context.token_env.clone(),
        }
    }
}

impl CliReport for ContextReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line(&format!("context '{}' {}", self.name, self.action))?;
        human.field("endpoint", &self.endpoint)?;
        human.field("token env", &self.token_env)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn store_round_trip_keeps_references_not_tokens() -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join(STORE_FILENAME);
        let mut store = ContextStore::default();
        store.add(
            "workstation",
            RemoteContext::new(
                "https://router.example/control/v1",
                "WORKSTATION_BITROUTER_TOKEN",
            )?,
        )?;
        store.save_to(&path)?;

        let text = std::fs::read_to_string(&path)?;
        assert!(text.contains("WORKSTATION_BITROUTER_TOKEN"));
        assert!(!text.contains("Bearer"));
        let loaded = ContextStore::load_from(&path)?;
        assert_eq!(loaded.get("workstation")?, store.get("workstation")?);
        Ok(())
    }

    #[test]
    fn insecure_non_loopback_http_is_rejected() {
        let error = RemoteContext::new(
            "http://router.example/control/v1",
            "WORKSTATION_BITROUTER_TOKEN",
        )
        .err()
        .map(|error| error.to_string());
        assert!(error.is_some_and(|message| message.contains("plain HTTP")));
    }

    #[test]
    fn local_is_reserved_and_not_persisted() -> anyhow::Result<()> {
        let mut store = ContextStore::default();
        let result = store.add(
            "local",
            RemoteContext {
                endpoint: "http://127.0.0.1:4358/control/v1/".to_string(),
                token_env: "BITROUTER_CONTROL_TOKEN".to_string(),
            },
        );
        assert!(result.is_err());
        assert!(resolve("local")?.is_none());
        Ok(())
    }
}
