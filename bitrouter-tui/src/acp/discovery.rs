use std::path::PathBuf;

use crate::model::{Agent, AgentStatus};

/// Known ACP agent entries: (display_name, binary_or_npx_args, is_npx).
///
/// Some agents have native ACP support (direct binary). Others require an
/// adapter package launched via `npx`.
const KNOWN_AGENTS: &[AgentSpec] = &[
    // Agents with native ACP support
    AgentSpec {
        name: "openclaw",
        bin: "openclaw",
        args: &["acp"],
        needs_npx: false,
    },
    AgentSpec {
        name: "gemini",
        bin: "gemini",
        args: &["--acp"],
        needs_npx: false,
    },
    AgentSpec {
        name: "copilot",
        bin: "copilot",
        args: &["--acp", "--stdio"],
        needs_npx: false,
    },
    // Agents requiring ACP adapter wrappers
    AgentSpec {
        name: "claude",
        bin: "claude-agent-acp",
        args: &[],
        needs_npx: false,
    },
    AgentSpec {
        name: "opencode",
        bin: "opencode-acp",
        args: &[],
        needs_npx: false,
    },
    AgentSpec {
        name: "codex",
        bin: "codex-acp",
        args: &[],
        needs_npx: false,
    },
];

struct AgentSpec {
    name: &'static str,
    bin: &'static str,
    args: &'static [&'static str],
    needs_npx: bool,
}

/// A discovered agent with its launch configuration.
#[derive(Debug, Clone)]
pub(crate) struct AgentLaunch {
    pub bin_path: PathBuf,
    pub args: Vec<String>,
}

/// Scan PATH for known ACP-compatible agent binaries.
pub(crate) fn discover_agents() -> Vec<Agent> {
    let path_var = match std::env::var_os("PATH") {
        Some(p) => p,
        None => return Vec::new(),
    };

    let dirs: Vec<PathBuf> = std::env::split_paths(&path_var).collect();
    let mut agents = Vec::new();

    for spec in KNOWN_AGENTS {
        let bin_name = if spec.needs_npx { "npx" } else { spec.bin };
        if let Some(bin_path) = find_in_dirs(bin_name, &dirs) {
            agents.push(Agent {
                name: spec.name.to_string(),
                launch: Some(AgentLaunch {
                    bin_path,
                    args: spec.args.iter().map(|s| (*s).to_string()).collect(),
                }),
                status: AgentStatus::Idle,
                session_id: None,
                color: ratatui::style::Color::White, // Re-assigned by App::new
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
