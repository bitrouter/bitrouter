//! `Send`-safe facade over an ACP agent connection.
//!
//! `AcpAgentProvider` hides the `!Send` ACP internals behind an mpsc
//! channel interface. The provider is `Send + Sync` and can be held
//! anywhere in the application.

use std::path::PathBuf;

use tokio::sync::mpsc;

use super::connection::spawn_agent_thread;
use super::types::{AgentCommand, AgentEvent};

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
    /// The binary is resolved from PATH if `config.binary` is a bare name.
    pub fn spawn(
        agent_id: String,
        config: &bitrouter_config::AgentConfig,
        event_tx: mpsc::Sender<AgentEvent>,
    ) -> Self {
        let bin_path = resolve_binary(&config.binary);
        let args = config.args.clone();

        let (handle, command_tx) = spawn_agent_thread(agent_id.clone(), bin_path, args, event_tx);

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

/// Resolve a binary name to a full path.
///
/// If the name contains a path separator, treat it as an absolute or
/// relative path. Otherwise, search PATH.
fn resolve_binary(name: &str) -> PathBuf {
    let path = PathBuf::from(name);
    if path.components().count() > 1 {
        return path;
    }

    // Search PATH
    if let Some(path_var) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path_var) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return candidate;
            }
        }
    }

    // Fall back to the bare name (will fail at spawn time with a clear error)
    path
}

// Compile-time assertion: AcpAgentProvider must be Send + Sync.
const _: () = {
    const fn _assert<T: Send + Sync>() {}
    _assert::<AcpAgentProvider>();
};
