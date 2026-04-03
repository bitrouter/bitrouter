//! `Send`-safe facade over an ACP agent connection.
//!
//! `AcpAgentProvider` hides the `!Send` ACP internals behind an mpsc
//! channel interface. The provider is `Send + Sync` and can be held
//! anywhere in the application.

use std::path::PathBuf;

use bitrouter_config::{AgentConfig, Distribution};
use tokio::sync::mpsc;

use super::connection::spawn_agent_thread;
use super::types::{AgentCommand, AgentEvent};

/// Resolved launch command for an agent subprocess.
pub(crate) struct LaunchCommand {
    pub binary: PathBuf,
    pub args: Vec<String>,
}

/// Send-safe handle to a running ACP agent connection.
///
/// Internally manages a dedicated OS thread with a single-threaded
/// tokio runtime and `LocalSet` (because ACP types are `!Send`).
/// All communication crosses the thread boundary via mpsc channels.
pub struct AcpAgentProvider {
    agent_id: String,
    command_tx: mpsc::Sender<AgentCommand>,
    thread_handle: Option<std::thread::JoinHandle<()>>,
}

impl AcpAgentProvider {
    /// Spawn a new ACP agent connection.
    ///
    /// - `agent_id`: display name for this agent
    /// - `config`: agent configuration from bitrouter-config
    /// - `event_tx`: channel where the consumer receives `AgentEvent`s
    ///
    /// Resolution order: binary on PATH, then first viable distribution
    /// method (npx/uvx), then bare binary name as fallback.
    pub fn spawn(
        agent_id: String,
        config: &AgentConfig,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Self {
        let launch = resolve_launch(config);

        let (handle, command_tx) =
            spawn_agent_thread(agent_id.clone(), launch.binary, launch.args, event_tx);

        Self {
            agent_id,
            command_tx,
            thread_handle: Some(handle),
        }
    }

    /// Send a prompt to the agent (async).
    pub async fn prompt(&self, text: String) -> Result<(), mpsc::error::SendError<AgentCommand>> {
        self.command_tx.send(AgentCommand::Prompt(text)).await
    }

    /// Send a prompt to the agent (non-blocking, best effort).
    pub fn try_prompt(&self, text: String) {
        let _ = self.command_tx.try_send(AgentCommand::Prompt(text));
    }

    /// Agent identifier.
    pub fn agent_id(&self) -> &str {
        &self.agent_id
    }
}

impl Drop for AcpAgentProvider {
    fn drop(&mut self) {
        // Dropping command_tx signals the agent thread to exit.
        // We intentionally do NOT join the thread here to avoid
        // blocking the caller. The thread will clean up on its own.
        drop(self.thread_handle.take());
    }
}

/// Resolve how to launch an agent based on config and distribution metadata.
///
/// 1. Binary on PATH → use directly
/// 2. First viable distribution (npx/uvx with runtime available)
/// 3. Bare binary name fallback (will fail at spawn with a clear error)
fn resolve_launch(config: &AgentConfig) -> LaunchCommand {
    // 1. Try PATH first.
    if let Some(path) = find_on_path(&config.binary) {
        return LaunchCommand {
            binary: path,
            args: config.args.clone(),
        };
    }

    // 2. Try distribution methods in order.
    for dist in &config.distribution {
        match dist {
            Distribution::Npx { package, args } => {
                if find_on_path("npx").is_some() {
                    let mut full_args = vec![package.clone()];
                    full_args.extend(args.iter().cloned());
                    return LaunchCommand {
                        binary: PathBuf::from("npx"),
                        args: full_args,
                    };
                }
            }
            Distribution::Uvx { package, args } => {
                if find_on_path("uvx").is_some() {
                    let mut full_args = vec![package.clone()];
                    full_args.extend(args.iter().cloned());
                    return LaunchCommand {
                        binary: PathBuf::from("uvx"),
                        args: full_args,
                    };
                }
            }
            Distribution::Binary { .. } => {
                // Binary distribution requires prior download — skip here.
                continue;
            }
        }
    }

    // 3. Fall back to bare name (will fail at spawn time with a clear error).
    LaunchCommand {
        binary: PathBuf::from(&config.binary),
        args: config.args.clone(),
    }
}

/// Search PATH for a binary name. Returns the full path if found.
fn find_on_path(name: &str) -> Option<PathBuf> {
    let path = PathBuf::from(name);
    if path.components().count() > 1 {
        return Some(path);
    }

    let path_var = std::env::var_os("PATH")?;
    for dir in std::env::split_paths(&path_var) {
        let candidate = dir.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
    }
    None
}

// Compile-time assertion: AcpAgentProvider must be Send + Sync.
const _: () = {
    const fn _assert<T: Send + Sync>() {}
    _assert::<AcpAgentProvider>();
};
