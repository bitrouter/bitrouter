//! `Send`-safe facade over an ACP agent connection.
//!
//! `AcpAgentProvider` implements the `AgentProvider` trait from
//! `bitrouter-core`. It hides the `!Send` ACP internals behind an mpsc
//! channel interface. The provider is `Send + Sync` and can be held
//! anywhere in the application.
//!
//! The provider supports multiple concurrent sessions, each backed by its
//! own subprocess and OS thread. Sessions are keyed by the protocol-assigned
//! session ID and tracked with a last-active timestamp for idle cleanup.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use std::time::{Duration, Instant};

use bitrouter_config::{AgentConfig, AgentSessionConfig, Distribution};
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

/// A single active ACP session within the provider's pool.
struct SessionEntry {
    command_tx: mpsc::Sender<AgentCommand>,
    _thread_handle: std::thread::JoinHandle<()>,
    last_active: Instant,
}

/// Send-safe handle to one or more ACP agent connections.
///
/// Internally manages a pool of sessions, each on a dedicated OS thread
/// with a single-threaded tokio runtime and `LocalSet` (because ACP types
/// are `!Send`). All communication crosses thread boundaries via mpsc
/// channels.
///
/// The pool enforces a configurable max-concurrency limit and tracks
/// per-session idle timestamps so that the runtime can periodically
/// reclaim stale sessions via [`cleanup_idle_sessions`](Self::cleanup_idle_sessions).
pub struct AcpAgentProvider {
    agent_name: String,
    config: AgentConfig,
    session_config: AgentSessionConfig,
    /// Active sessions keyed by protocol-assigned session ID.
    sessions: Mutex<HashMap<String, SessionEntry>>,
}

impl AcpAgentProvider {
    /// Create a new provider for the given agent.
    ///
    /// This does **not** spawn any subprocess — call
    /// [`connect`](AgentProvider::connect) to establish a session.
    pub fn new(agent_name: String, config: AgentConfig) -> Self {
        let session_config = config.session.as_ref().cloned().unwrap_or_default();
        Self {
            agent_name,
            config,
            session_config,
            sessions: Mutex::new(HashMap::new()),
        }
    }

    /// Returns the configured idle timeout for sessions.
    pub fn idle_timeout(&self) -> Duration {
        Duration::from_secs(self.session_config.idle_timeout_secs)
    }

    /// Returns the maximum number of concurrent sessions allowed.
    pub fn max_concurrent(&self) -> usize {
        self.session_config.max_concurrent
    }

    /// Returns the number of currently active sessions.
    pub fn session_count(&self) -> usize {
        self.sessions.lock().map(|s| s.len()).unwrap_or(0)
    }

    /// Remove sessions that have been idle longer than the configured timeout.
    ///
    /// Returns the number of sessions cleaned up. Sends a graceful
    /// [`Disconnect`](AgentCommand::Disconnect) to each removed session.
    pub async fn cleanup_idle_sessions(&self) -> usize {
        let idle_timeout = self.idle_timeout();
        let now = Instant::now();

        let to_cleanup: Vec<mpsc::Sender<AgentCommand>> = {
            let mut sessions = match self.sessions.lock() {
                Ok(s) => s,
                Err(_) => return 0,
            };
            let mut senders = Vec::new();
            let mut ids_to_remove = Vec::new();
            for (id, entry) in sessions.iter() {
                if now.duration_since(entry.last_active) > idle_timeout {
                    senders.push(entry.command_tx.clone());
                    ids_to_remove.push(id.clone());
                }
            }
            for id in &ids_to_remove {
                sessions.remove(id);
            }
            senders
        };

        let count = to_cleanup.len();
        for tx in to_cleanup {
            let _ = tx.send(AgentCommand::Disconnect).await;
        }
        count
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
        // Enforce max concurrency.
        {
            let sessions = self.sessions.lock().map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "session lock poisoned")
            })?;
            let max = self.session_config.max_concurrent;
            if sessions.len() >= max {
                return Err(BitrouterError::transport(
                    Some(&self.agent_name),
                    format!("max concurrent sessions ({max}) reached"),
                ));
            }
        }

        let launch = resolve_launch(&self.config);
        let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel();

        let thread_handle = spawn_agent_thread(
            self.agent_name.clone(),
            launch.binary,
            launch.args,
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

        {
            let mut sessions = self.sessions.lock().map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "session lock poisoned")
            })?;
            sessions.insert(
                session_info.session_id.clone(),
                SessionEntry {
                    command_tx,
                    _thread_handle: thread_handle,
                    last_active: Instant::now(),
                },
            );
        }

        Ok(session_info)
    }

    async fn submit(&self, session_id: &str, text: String) -> Result<mpsc::Receiver<AgentEvent>> {
        let command_tx = {
            let mut sessions = self.sessions.lock().map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "session lock poisoned")
            })?;
            match sessions.get_mut(session_id) {
                Some(entry) => {
                    entry.last_active = Instant::now();
                    entry.command_tx.clone()
                }
                None => {
                    return Err(BitrouterError::transport(
                        Some(&self.agent_name),
                        format!("session '{session_id}' not found — call connect() first"),
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
        session_id: &str,
        request_id: PermissionRequestId,
        response: PermissionResponse,
    ) -> Result<()> {
        let command_tx = {
            let mut sessions = self.sessions.lock().map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "session lock poisoned")
            })?;
            match sessions.get_mut(session_id) {
                Some(entry) => {
                    entry.last_active = Instant::now();
                    entry.command_tx.clone()
                }
                None => {
                    return Err(BitrouterError::transport(
                        Some(&self.agent_name),
                        format!("session '{session_id}' not found"),
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

    async fn disconnect(&self, session_id: &str) -> Result<()> {
        let command_tx = {
            let mut sessions = self.sessions.lock().map_err(|_| {
                BitrouterError::transport(Some(&self.agent_name), "session lock poisoned")
            })?;
            sessions.remove(session_id).map(|entry| entry.command_tx)
        };

        if let Some(tx) = command_tx {
            let _ = tx.send(AgentCommand::Disconnect).await;
        }

        Ok(())
    }
}

impl Drop for AcpAgentProvider {
    fn drop(&mut self) {
        // Dropping the sessions map drops all command_tx senders,
        // signaling agent threads to exit. We intentionally do NOT
        // join threads here to avoid blocking the caller.
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_config::{AgentConfig, AgentProtocol, AgentSessionConfig};

    fn make_config(session: Option<AgentSessionConfig>) -> AgentConfig {
        AgentConfig {
            protocol: AgentProtocol::Acp,
            binary: "nonexistent-agent-binary".to_owned(),
            args: Vec::new(),
            enabled: true,
            distribution: Vec::new(),
            session,
            a2a: None,
        }
    }

    #[test]
    fn provider_defaults_to_single_session() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        assert_eq!(provider.max_concurrent(), 1);
        assert_eq!(provider.idle_timeout(), Duration::from_secs(600));
        assert_eq!(provider.session_count(), 0);
    }

    #[test]
    fn provider_respects_session_config() {
        let config = make_config(Some(AgentSessionConfig {
            idle_timeout_secs: 120,
            max_concurrent: 8,
        }));
        let provider = AcpAgentProvider::new("test".to_owned(), config);
        assert_eq!(provider.max_concurrent(), 8);
        assert_eq!(provider.idle_timeout(), Duration::from_secs(120));
    }

    #[test]
    fn provider_agent_name() {
        let provider = AcpAgentProvider::new("claude-code".to_owned(), make_config(None));
        assert_eq!(provider.agent_name(), "claude-code");
        assert_eq!(provider.protocol_name(), "acp");
    }

    #[tokio::test]
    async fn submit_without_connect_errors() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        let result = provider
            .submit("nonexistent-session", "hello".to_owned())
            .await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn disconnect_unknown_session_is_noop() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        // Disconnecting a session that doesn't exist should succeed silently.
        let result = provider.disconnect("nonexistent-session").await;
        assert!(result.is_ok());
    }

    #[tokio::test]
    async fn cleanup_idle_sessions_empty_pool() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        let cleaned = provider.cleanup_idle_sessions().await;
        assert_eq!(cleaned, 0);
    }
}
