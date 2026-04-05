//! Agent binary discovery — scan PATH for known ACP-compatible agents.

use std::collections::HashMap;
use std::path::PathBuf;

use bitrouter_config::AgentConfig;

use super::types::{AgentAvailability, DiscoveredAgent};

/// Scan PATH for agent binaries defined in `known`.
///
/// Returns a `DiscoveredAgent` for each agent that is either found on
/// PATH or has distribution metadata for auto-install. Agents with
/// neither are omitted.
///
/// This does **not** decide whether the agent is enabled — that's
/// the caller's responsibility.
pub fn discover_agents(known: &HashMap<String, AgentConfig>) -> Vec<DiscoveredAgent> {
    let path_var = std::env::var_os("PATH");
    let dirs: Vec<PathBuf> = path_var
        .as_ref()
        .map(|p| std::env::split_paths(p).collect())
        .unwrap_or_default();

    let mut agents = Vec::new();

    for (name, config) in known {
        let availability = if let Some(bin_path) = find_in_dirs(&config.binary, &dirs) {
            AgentAvailability::OnPath(bin_path)
        } else if !config.distribution.is_empty() {
            AgentAvailability::Distributable
        } else {
            continue; // Neither on PATH nor distributable — skip.
        };

        let binary = match &availability {
            AgentAvailability::OnPath(p) => p.clone(),
            AgentAvailability::Distributable => PathBuf::from(&config.binary),
        };

        agents.push(DiscoveredAgent {
            name: name.clone(),
            binary,
            args: config.args.clone(),
            availability,
        });
    }

    agents
}

fn find_in_dirs(name: &str, dirs: &[PathBuf]) -> Option<PathBuf> {
    for dir in dirs {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}
