//! High-level ACP agent operations shared by the CLI and TUI.
//!
//! Each function returns structured, format-agnostic data. The caller
//! decides how to render it. Long-running operations return an [`OpHandle`]
//! carrying a progress channel and a task handle.

use std::collections::HashMap;
use std::path::PathBuf;

use bitrouter_config::BitrouterConfig;
use bitrouter_config::acp::registry_agent_to_config;
use serde::Serialize;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

use super::eager::{self, InstalledAgent};
use super::registry;
use super::state::{self, InstallRecord};
use super::types::InstallProgress;

/// Paths needed by ACP operations, grouped so callers don't need to import
/// the binary's path module.
#[derive(Debug, Clone)]
pub struct AcpPaths {
    pub cache_dir: PathBuf,
    pub agents_dir: PathBuf,
    pub agent_state_file: PathBuf,
}

/// Error type for ACP operations.
#[derive(Debug, thiserror::Error)]
pub enum OpError {
    #[error("not found: {kind} '{id}'")]
    NotFound { kind: &'static str, id: String },

    #[error("registry: {0}")]
    Registry(String),

    #[error("install: {0}")]
    Install(String),

    #[error("uninstall: {0}")]
    Uninstall(String),

    #[error("io: {0}")]
    Io(#[from] std::io::Error),
}

impl OpError {
    /// Stable CLI exit-code mapping.
    pub fn exit_code(&self) -> i32 {
        match self {
            OpError::NotFound { .. } => 4,
            OpError::Registry(_) => 8,
            OpError::Install(_) | OpError::Uninstall(_) => 9,
            OpError::Io(_) => 1,
        }
    }
}

/// Handle for a long-running agent operation.
///
/// Read [`progress`] events until `None` (channel closed), then await
/// [`result`] for the final outcome.
pub struct OpHandle<R> {
    pub progress: mpsc::Receiver<InstallProgress>,
    pub result: JoinHandle<Result<R, OpError>>,
}

/// Summary of a single agent across registry, config, and install state.
#[derive(Debug, Clone, Serialize)]
pub struct AgentInfo {
    pub id: String,
    pub version: Option<String>,
    pub installed: Option<InstallRecord>,
    pub on_path: bool,
    pub from_registry: bool,
}

/// Full result of the `list_agents` operation.
#[derive(Debug, Clone, Serialize)]
pub struct AgentList {
    pub registry_version: Option<String>,
    pub registry_url: String,
    pub agents: Vec<AgentInfo>,
    pub warnings: Vec<String>,
}

/// Result of the `check_routing` operation.
#[derive(Debug, Clone, Serialize)]
pub struct RoutingCheck {
    pub server_reachable: bool,
    pub listen_addr: String,
    pub shim_entries: Vec<ShimEntry>,
    pub discovered_on_path: Vec<String>,
    pub discovered_distributable: Vec<String>,
}

/// Shim installation status for one agent.
#[derive(Debug, Clone, Serialize)]
pub struct ShimEntry {
    pub agent_id: String,
    pub shim_installed: bool,
    pub shim_path: PathBuf,
}

/// Merge registry + config + install state into a structured agent list.
pub async fn list_agents(
    config: &BitrouterConfig,
    paths: &AcpPaths,
    refresh: bool,
) -> Result<AgentList, OpError> {
    let cache_file = paths.cache_dir.join("acp-registry.json");
    let registry_url = registry::resolve_registry_url(config.acp_registry_url.as_deref());

    let mut warnings = Vec::new();
    let registry_result = if refresh {
        registry::fetch_registry_fresh(&cache_file, &registry_url).await
    } else {
        registry::fetch_registry(&cache_file, registry::DEFAULT_TTL_SECS, &registry_url).await
    };

    let mut known = bitrouter_config::builtin_agent_defs();
    for (name, agent_config) in &config.agents {
        known.insert(name.clone(), agent_config.clone());
    }

    let (registry_version, registry_map) = match registry_result {
        Ok(index) => {
            let mut map = HashMap::new();
            for agent in &index.agents {
                known.insert(agent.id.clone(), registry_agent_to_config(agent));
                map.insert(agent.id.clone(), agent.clone());
            }
            (Some(index.version), map)
        }
        Err(e) => {
            warnings.push(format!("registry unavailable: {e}"));
            (None, HashMap::new())
        }
    };

    state::overlay_install_state_sync(&mut known, &paths.agent_state_file);
    let records: HashMap<String, InstallRecord> = state::load_state_sync(&paths.agent_state_file)
        .into_iter()
        .map(|r| (r.id.clone(), r))
        .collect();

    let discovered = super::discovery::discover_agents(&known);

    let mut names: Vec<_> = known.keys().cloned().collect();
    names.sort();

    let agents = names
        .into_iter()
        .map(|name| {
            let on_path = discovered.iter().any(|d| d.name == name);
            let installed = records.get(&name).cloned();
            let from_registry = registry_map.contains_key(&name);
            let version = registry_map.get(&name).map(|r| r.version.clone());
            AgentInfo {
                id: name,
                version,
                installed,
                on_path,
                from_registry,
            }
        })
        .collect();

    Ok(AgentList {
        registry_version,
        registry_url,
        agents,
        warnings,
    })
}

/// Install an agent by id from the ACP registry.
///
/// Returns an [`OpHandle`] immediately. Read progress events until `None`,
/// then await `result` for the installation outcome.
pub fn install_agent(
    agent_id: &str,
    config: &BitrouterConfig,
    paths: &AcpPaths,
) -> OpHandle<InstalledAgent> {
    let agent_id = agent_id.to_owned();
    let cache_file = paths.cache_dir.join("acp-registry.json");
    let install_dir = paths.agents_dir.join(&agent_id);
    let state_file = paths.agent_state_file.clone();
    let registry_url = registry::resolve_registry_url(config.acp_registry_url.as_deref());

    let (progress_tx, progress_rx) = mpsc::channel(32);

    let result = tokio::spawn(async move {
        let index =
            registry::fetch_registry(&cache_file, registry::DEFAULT_TTL_SECS, &registry_url)
                .await
                .map_err(OpError::Registry)?;

        let registry_agent = index
            .agents
            .iter()
            .find(|a| a.id == agent_id)
            .ok_or_else(|| OpError::NotFound {
                kind: "agent",
                id: agent_id.clone(),
            })?;

        let agent_config = registry_agent_to_config(registry_agent);
        let version = registry_agent.version.clone();

        eager::install_agent(
            &agent_id,
            &agent_config,
            &install_dir,
            &state_file,
            &version,
            progress_tx,
        )
        .await
        .map_err(OpError::Install)
    });

    OpHandle {
        progress: progress_rx,
        result,
    }
}

/// Uninstall a previously installed agent.
pub async fn uninstall_agent(agent_id: &str, paths: &AcpPaths) -> Result<(), OpError> {
    let install_dir = paths.agents_dir.join(agent_id);
    eager::uninstall_agent(agent_id, &install_dir, &paths.agent_state_file)
        .await
        .map_err(OpError::Uninstall)
}

/// Update one or all installed agents by reinstalling from the registry.
///
/// Progress events are discarded. Call [`install_agent`] directly when
/// per-agent progress streaming is needed.
pub async fn update_agents(
    target: Option<&str>,
    config: &BitrouterConfig,
    paths: &AcpPaths,
) -> Result<Vec<InstalledAgent>, OpError> {
    let records = state::load_state_sync(&paths.agent_state_file);

    let targets: Vec<String> = match target {
        Some(id) => {
            if !records.iter().any(|r| r.id == id) {
                return Err(OpError::NotFound {
                    kind: "installed agent",
                    id: id.to_owned(),
                });
            }
            vec![id.to_owned()]
        }
        None => records.iter().map(|r| r.id.clone()).collect(),
    };

    let mut results = Vec::new();
    for id in &targets {
        let mut handle = install_agent(id, config, paths);
        while handle.progress.recv().await.is_some() {}
        let installed = handle
            .result
            .await
            .map_err(|e| OpError::Install(e.to_string()))??;
        results.push(installed);
    }
    Ok(results)
}

/// Check that agent routing through BitRouter is properly configured.
pub fn check_routing(config: &BitrouterConfig) -> RoutingCheck {
    let listen = config.server.listen;

    let mut known = bitrouter_config::builtin_agent_defs();
    for (name, agent_config) in &config.agents {
        known.insert(name.clone(), agent_config.clone());
    }

    let discovered_agents = super::discovery::discover_agents(&known);

    let shim_dir = dirs::home_dir()
        .map(|h| h.join(".local").join("bin"))
        .unwrap_or_else(|| PathBuf::from(".local/bin"));
    let platform = super::shim::Platform::current();

    let shim_entries: Vec<ShimEntry> = discovered_agents
        .iter()
        .filter(|a| super::shim::shim_env_for(&a.name, listen).is_some())
        .map(|a| {
            let shim_path = super::shim::shim_path_for(platform, &shim_dir, &a.name);
            let shim_installed = super::shim::is_installed(&shim_path);
            ShimEntry {
                agent_id: a.name.clone(),
                shim_installed,
                shim_path,
            }
        })
        .collect();

    let server_reachable =
        std::net::TcpStream::connect_timeout(&listen, std::time::Duration::from_secs(2)).is_ok();

    use super::types::AgentAvailability;
    let discovered_on_path: Vec<String> = discovered_agents
        .iter()
        .filter(|a| matches!(a.availability, AgentAvailability::OnPath(_)))
        .map(|a| a.name.clone())
        .collect();
    let discovered_distributable: Vec<String> = discovered_agents
        .iter()
        .filter(|a| matches!(a.availability, AgentAvailability::Distributable))
        .map(|a| a.name.clone())
        .collect();

    RoutingCheck {
        server_reachable,
        listen_addr: listen.to_string(),
        shim_entries,
        discovered_on_path,
        discovered_distributable,
    }
}
