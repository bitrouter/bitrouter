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
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use bitrouter_config::{AgentConfig, AgentSessionConfig, Distribution};
use bitrouter_core::agents::event::{AgentEvent, PermissionRequestId, PermissionResponse};
use bitrouter_core::agents::provider::AgentProvider;
use bitrouter_core::agents::session::{AgentCapabilities, AgentSessionInfo};
use bitrouter_core::errors::{BitrouterError, Result};
use tokio::sync::{OwnedSemaphorePermit, Semaphore, mpsc};

use super::connection::{HandshakeResult, InitMode, spawn_agent_thread};
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
    /// Held for the lifetime of the session. Dropping it releases the
    /// concurrency slot in the provider's semaphore.
    _permit: OwnedSemaphorePermit,
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
    /// Limits concurrent sessions. Initialized with `max_concurrent`
    /// permits; each active session (and in-flight connect) holds one.
    connect_semaphore: Arc<Semaphore>,
    /// Most recently observed capability set, captured from the agent's
    /// `initialize` response. `None` until the first successful
    /// handshake; the `capabilities()` accessor returns
    /// `Default::default()` (all flags false) in that window so callers
    /// can gate import flows without special-casing the cold start.
    cached_capabilities: Mutex<Option<AgentCapabilities>>,
}

impl AcpAgentProvider {
    /// Create a new provider for the given agent.
    ///
    /// This does **not** spawn any subprocess — call
    /// [`connect`](AgentProvider::connect) to establish a session.
    pub fn new(agent_name: String, config: AgentConfig) -> Self {
        let session_config = config.session.as_ref().cloned().unwrap_or_default();
        let connect_semaphore = Arc::new(Semaphore::new(session_config.max_concurrent));
        Self {
            agent_name,
            config,
            session_config,
            sessions: Mutex::new(HashMap::new()),
            connect_semaphore,
            cached_capabilities: Mutex::new(None),
        }
    }

    /// Persist the capabilities observed at handshake time. Called from
    /// `connect` and `load_session` once the ACP `initialize` response
    /// has been parsed.
    fn record_capabilities(&self, caps: &AgentCapabilities) {
        if let Ok(mut slot) = self.cached_capabilities.lock() {
            *slot = Some(caps.clone());
        }
    }

    /// Spawn a fresh agent thread, run the initialize handshake, and
    /// register the resulting session in the pool. Used by both
    /// `connect` (`InitMode::New`) and `load_session`
    /// (`InitMode::Load`); the only difference is the second ACP call
    /// and any replay-stream wiring, both handled inside the thread.
    async fn spawn_initialised_session(
        &self,
        cwd: &Path,
        init_mode: InitMode,
    ) -> Result<AgentSessionInfo> {
        if !cwd.is_absolute() {
            return Err(BitrouterError::transport(
                Some(&self.agent_name),
                format!("cwd must be absolute: {}", cwd.display()),
            ));
        }

        // Atomically reserve a concurrency slot. The permit is held for
        // the lifetime of the session (stored in SessionEntry) and
        // released when the session is removed.
        let permit = Arc::clone(&self.connect_semaphore)
            .try_acquire_owned()
            .map_err(|_| {
                let max = self.session_config.max_concurrent;
                BitrouterError::transport(
                    Some(&self.agent_name),
                    format!("max concurrent sessions ({max}) reached"),
                )
            })?;

        let launch = resolve_launch(&self.config);
        let (handshake_tx, handshake_rx) = tokio::sync::oneshot::channel();

        let thread_handle = spawn_agent_thread(
            self.agent_name.clone(),
            launch.binary,
            launch.args,
            cwd.to_path_buf(),
            init_mode,
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

        self.record_capabilities(&session_info.capabilities);

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
                    _permit: permit,
                },
            );
        }

        Ok(session_info)
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
    /// Dropping the removed entries releases their semaphore permits,
    /// freeing concurrency slots for new connections.
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

    async fn connect(&self, cwd: &Path) -> Result<AgentSessionInfo> {
        self.spawn_initialised_session(cwd, InitMode::New).await
    }

    async fn load_session(
        &self,
        cwd: &Path,
        external_id: &str,
    ) -> Result<(AgentSessionInfo, mpsc::Receiver<AgentEvent>)> {
        // Cold-start guard: we only have cached capabilities after the
        // first successful handshake. If the cache is empty we let the
        // load_session attempt go through — the agent will reject with
        // method_not_found if it doesn't actually support load.
        let caps = self.capabilities();
        let caps_known = self
            .cached_capabilities
            .lock()
            .map(|g| g.is_some())
            .unwrap_or(false);
        if caps_known && !caps.load_session {
            return Err(BitrouterError::transport(
                Some(&self.agent_name),
                "agent does not advertise session/load capability",
            ));
        }
        let (replay_tx, replay_rx) = mpsc::channel(64);
        let mode = InitMode::Load {
            external_id: external_id.to_string(),
            replay_tx,
        };
        let info = self.spawn_initialised_session(cwd, mode).await?;
        Ok((info, replay_rx))
    }

    fn capabilities(&self) -> AgentCapabilities {
        self.cached_capabilities
            .lock()
            .ok()
            .and_then(|g| g.clone())
            .unwrap_or_default()
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
    fn provider_defaults_to_eight_concurrent_sessions() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        assert_eq!(provider.max_concurrent(), 8);
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
    async fn connect_rejects_relative_cwd() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        let result = provider
            .connect(std::path::Path::new("relative/path"))
            .await;
        let err = result.expect_err("relative cwd must error");
        assert!(
            format!("{err}").contains("absolute"),
            "expected error to mention absolute, got: {err}"
        );
    }

    #[tokio::test]
    async fn connect_rejects_empty_cwd() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        let result = provider.connect(std::path::Path::new("")).await;
        assert!(result.is_err(), "empty cwd must error");
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

    #[test]
    fn capabilities_default_to_all_false_before_handshake() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        let caps = provider.capabilities();
        assert!(!caps.load_session);
        assert!(!caps.prompt_image);
        assert!(!caps.prompt_audio);
    }

    #[tokio::test]
    async fn load_session_fails_when_capability_known_false() {
        let provider = AcpAgentProvider::new("test".to_owned(), make_config(None));
        // Force a known-but-empty capability set so load_session
        // takes the fast-fail branch instead of trying to spawn.
        provider.record_capabilities(&AgentCapabilities::default());
        let result = provider
            .load_session(std::path::Path::new("/tmp"), "ext-id")
            .await;
        let err = result.expect_err("load without capability must fail");
        assert!(
            format!("{err}").contains("session/load"),
            "expected error to mention session/load, got: {err}"
        );
    }
}
