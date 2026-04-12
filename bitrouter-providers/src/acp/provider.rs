//! `Send`-safe facade over an ACP agent connection.
//!
//! `AcpAgentProvider` implements the `AgentProvider` trait from
//! `bitrouter-core`. It hides the `!Send` ACP internals behind an mpsc
//! channel interface. The provider is `Send + Sync` and can be held
//! anywhere in the application.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;

use bitrouter_config::{AgentConfig, Distribution};
use bitrouter_core::agents::event::{AgentEvent, PermissionRequestId, PermissionResponse};
use bitrouter_core::agents::provider::AgentProvider;
use bitrouter_core::agents::session::AgentSessionInfo;
use bitrouter_core::errors::{BitrouterError, Result};
use tokio::sync::mpsc;

use super::connection::{HandshakeResult, spawn_agent_thread};
use super::types::AgentCommand;

/// Resolved launch command for an agent subprocess.
pub(crate) struct LaunchCommand {
    pub binary: PathBuf,
    pub args: Vec<String>,
}

/// Send-safe handle to an ACP agent connection.
///
/// Internally manages a dedicated OS thread with a single-threaded
/// tokio runtime and `LocalSet` (because ACP types are `!Send`).
/// All communication crosses the thread boundary via mpsc channels.
pub struct AcpAgentProvider {
    agent_name: String,
    config: AgentConfig,
    /// Resolved routing env vars to inject into the subprocess.
    routing_env: HashMap<String, String>,
    /// Command channel to the agent thread. Set after `connect`.
    state: Mutex<ConnectionState>,
}

enum ConnectionState {
    /// Not yet connected.
    Idle,
    /// Connected to the agent subprocess.
    Connected {
        command_tx: mpsc::Sender<AgentCommand>,
        _thread_handle: std::thread::JoinHandle<()>,
    },
}

impl AcpAgentProvider {
    /// Create a new provider for the given agent.
    ///
    /// This does **not** spawn the subprocess — call
    /// [`connect`](AgentProvider::connect) to establish the session.
    ///
    /// `routing_env` contains resolved environment variables that will be
    /// injected into the agent subprocess to redirect LLM traffic through
    /// BitRouter. Pass an empty map to skip routing injection.
    pub fn new(
        agent_name: String,
        config: AgentConfig,
        routing_env: HashMap<String, String>,
    ) -> Self {
        Self {
            agent_name,
            config,
            routing_env,
            state: Mutex::new(ConnectionState::Idle),
        }
    }
}

impl AgentProvider for AcpAgentProvider {
    fn agent_name(&self) -> &str {
        &self.agent_name
    }

    fn protocol_name(&self) -> &str {
        "acp"
    }

    async fn connect(&self) -> Result<AgentSessionInfo> {
        let launch = resolve_launch(&self.config);
        let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel();

        let thread_handle = spawn_agent_thread(
            self.agent_name.clone(),
            launch.binary,
            launch.args,
            self.routing_env.clone(),
            handshake_tx,
        );

        let handshake = handshake_rx.await.map_err(|_| {
            BitrouterError::transport(
                Some(&self.agent_name),
                "agent thread exited before handshake",
            )
        })?;

        let HandshakeResult {
            session_info,
            command_tx,
        } = handshake.map_err(|msg| BitrouterError::transport(Some(&self.agent_name), msg))?;

        let mut state = self.state.lock().map_err(|_| {
            BitrouterError::transport(Some(&self.agent_name), "state lock poisoned")
        })?;
        *state = ConnectionState::Connected {
            command_tx,
            _thread_handle: thread_handle,
        };

        Ok(session_info)
    }

    async fn submit(&self, _session_id: &str, text: String) -> Result<mpsc::Receiver<AgentEvent>> {
        let command_tx = {
            let state = self.state.lock().map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "state lock poisoned")
            })?;
            match &*state {
                ConnectionState::Connected { command_tx, .. } => command_tx.clone(),
                ConnectionState::Idle => {
                    return Err(BitrouterError::transport(
                        Some(&self.agent_name),
                        "agent not connected — call connect() first",
                    ));
                }
            }
        };

        let (reply_tx, reply_rx) = mpsc::channel(64);

        command_tx
            .send(AgentCommand::Prompt { text, reply_tx })
            .await
            .map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "agent thread not running")
            })?;

        Ok(reply_rx)
    }

    async fn respond_permission(
        &self,
        _session_id: &str,
        request_id: PermissionRequestId,
        response: PermissionResponse,
    ) -> Result<()> {
        let command_tx = {
            let state = self.state.lock().map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "state lock poisoned")
            })?;
            match &*state {
                ConnectionState::Connected { command_tx, .. } => command_tx.clone(),
                ConnectionState::Idle => {
                    return Err(BitrouterError::transport(
                        Some(&self.agent_name),
                        "agent not connected",
                    ));
                }
            }
        };

        command_tx
            .send(AgentCommand::RespondPermission {
                request_id,
                response,
            })
            .await
            .map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "agent thread not running")
            })?;

        Ok(())
    }

    async fn disconnect(&self, _session_id: &str) -> Result<()> {
        let command_tx = {
            let mut state = self.state.lock().map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "state lock poisoned")
            })?;
            match std::mem::replace(&mut *state, ConnectionState::Idle) {
                ConnectionState::Connected {
                    command_tx,
                    _thread_handle,
                } => {
                    // Thread handle is dropped here, which is fine —
                    // the thread will exit after receiving Disconnect
                    // or when the command channel closes.
                    Some(command_tx)
                }
                ConnectionState::Idle => None,
            }
        };

        if let Some(tx) = command_tx {
            let _ = tx.send(AgentCommand::Disconnect).await;
        }

        Ok(())
    }
}

impl Drop for AcpAgentProvider {
    fn drop(&mut self) {
        // Dropping the command_tx signals the agent thread to exit.
        // We intentionally do NOT join the thread here to avoid
        // blocking the caller.
    }
}

/// Resolve how to launch an agent based on config and distribution metadata.
///
/// 1. Binary on PATH -> use directly
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
                continue;
            }
        }
    }

    // 3. Fall back to bare name.
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
