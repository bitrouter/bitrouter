//! Agent binary discovery — scan PATH for known ACP-compatible agents.

use std::collections::HashMap;
use std::path::PathBuf;

use bitrouter_config::AgentConfig;

use super::types::DiscoveredAgent;

/// Scan PATH for agent binaries defined in `known`.
///
/// Returns a `DiscoveredAgent` for each binary found on PATH.
/// This does **not** decide whether the agent is enabled — that's
/// the caller's responsibility (typically: check if it's already
/// in the user's config).
pub fn discover_agents(known: &HashMap<String, AgentConfig>) -> Vec<DiscoveredAgent> {
    let path_var = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return Vec::new(),
    };

    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    let mut agents = Vec::new();

    for (name, config) in known {
        if let Some(bin_path) = find_in_dirs(&config.binary, &dirs) {
            agents.push(DiscoveredAgent {
                name: name.clone(),
                binary: bin_path,
                args: config.args.clone(),
            });
        }
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
