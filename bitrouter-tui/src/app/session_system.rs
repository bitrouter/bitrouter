//! Session-lifecycle facade.
//!
//! Centralizes the four ACP lifecycle operations (connect, prompt,
//! disconnect, respond-permission) and owns the per-agent provider
//! handles. Previously these were four scattered `tokio::spawn` sites
//! across `agent_lifecycle.rs`; routing through `SessionSystem` gives
//! us one place to add SessionId-keyed dispatch when PR 4 introduces
//! multiple sessions per agent.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use bitrouter_config::AgentConfig;
use bitrouter_core::agents::event::{AgentEvent, PermissionRequestId, PermissionResponse};
use bitrouter_core::agents::provider::AgentProvider;
use bitrouter_providers::acp::provider::AcpAgentProvider;
use tokio::sync::mpsc;

use crate::event::AppEvent;

/// Owns active ACP providers and the async work that drives them.
pub(crate) struct SessionSystem {
    providers: HashMap<String, Arc<AcpAgentProvider>>,
    event_tx: mpsc::Sender<AppEvent>,
    launch_cwd: PathBuf,
}

impl SessionSystem {
    pub fn new(event_tx: mpsc::Sender<AppEvent>, launch_cwd: PathBuf) -> Self {
        Self {
            providers: HashMap::new(),
            event_tx,
            launch_cwd,
        }
    }

    pub fn has_provider(&self, agent_id: &str) -> bool {
        self.providers.contains_key(agent_id)
    }

    /// Spawn a fresh ACP provider for `agent_id` and drive the handshake
    /// on a background task. Returns `false` if a provider is already
    /// registered (the caller should not double-connect).
    pub fn spawn_connect(&mut self, agent_id: &str, config: &AgentConfig) -> bool {
        if self.providers.contains_key(agent_id) {
            return false;
        }

        let provider = Arc::new(AcpAgentProvider::new(agent_id.to_string(), config.clone()));
        self.providers
            .insert(agent_id.to_string(), provider.clone());

        let agent_id_owned = agent_id.to_string();
        let event_tx = self.event_tx.clone();
        let cwd = self.launch_cwd.clone();

        tokio::spawn(async move {
            match provider.connect(&cwd).await {
                Ok(info) => {
                    let _ = event_tx
                        .send(AppEvent::AgentConnected {
                            agent_id: agent_id_owned,
                            session_id: info.session_id,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = event_tx
                        .send(AppEvent::Agent(
                            agent_id_owned,
                            AgentEvent::Error {
                                message: format!("{e}"),
                            },
                        ))
                        .await;
                }
            }
        });
        true
    }

    /// Send a prompt to an existing session and forward the turn's
    /// stream of `AgentEvent`s to the app.
    pub fn send_prompt(&self, agent_id: &str, session_id: &str, text: String) {
        let Some(provider) = self.providers.get(agent_id).cloned() else {
            return;
        };
        let agent_id_owned = agent_id.to_string();
        let session_id_owned = session_id.to_string();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            match provider.submit(&session_id_owned, text).await {
                Ok(mut rx) => {
                    while let Some(evt) = rx.recv().await {
                        if event_tx
                            .send(AppEvent::Agent(agent_id_owned.clone(), evt))
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = event_tx
                        .send(AppEvent::Agent(
                            agent_id_owned,
                            AgentEvent::Error {
                                message: format!("{e}"),
                            },
                        ))
                        .await;
                }
            }
        });
    }

    /// Send `disconnect` and drop the provider handle. The agent thread
    /// will emit `AgentDisconnected` once the subprocess exits.
    pub fn disconnect(&mut self, agent_id: &str, session_id: &str) {
        if let Some(provider) = self.providers.remove(agent_id) {
            let session_id_owned = session_id.to_string();
            tokio::spawn(async move {
                let _ = provider.disconnect(&session_id_owned).await;
            });
        }
    }

    /// Drop the provider handle without sending `disconnect`. Used after
    /// the agent reports `AgentDisconnected` on its own (process exit,
    /// crash, etc.) so we don't race a no-op disconnect on the way out.
    pub fn forget(&mut self, agent_id: &str) {
        self.providers.remove(agent_id);
    }

    /// Resolve a pending permission request on the agent side.
    pub fn respond_permission(
        &self,
        agent_id: &str,
        session_id: &str,
        request_id: PermissionRequestId,
        response: PermissionResponse,
    ) {
        let Some(provider) = self.providers.get(agent_id).cloned() else {
            return;
        };
        let session_id_owned = session_id.to_string();
        tokio::spawn(async move {
            let _ = provider
                .respond_permission(&session_id_owned, request_id, response)
                .await;
        });
    }

    /// Drop all providers (shutdown path). Agent threads exit when their
    /// command-tx senders drop.
    pub fn shutdown(&mut self) {
        self.providers.clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_config::{AgentConfig, AgentProtocol};

    fn mk_config() -> AgentConfig {
        AgentConfig {
            protocol: AgentProtocol::Acp,
            binary: "nonexistent-agent-for-tests".to_owned(),
            args: Vec::new(),
            enabled: true,
            distribution: Vec::new(),
            session: None,
            a2a: None,
        }
    }

    fn mk_system() -> (SessionSystem, mpsc::Receiver<AppEvent>) {
        let (tx, rx) = mpsc::channel(16);
        let cwd = std::env::current_dir().expect("cwd available");
        (SessionSystem::new(tx, cwd), rx)
    }

    #[tokio::test]
    async fn has_provider_false_initially() {
        let (sys, _rx) = mk_system();
        assert!(!sys.has_provider("claude-code"));
    }

    #[tokio::test]
    async fn spawn_connect_inserts_provider_and_is_idempotent() {
        let (mut sys, _rx) = mk_system();
        let first = sys.spawn_connect("claude-code", &mk_config());
        assert!(first, "first spawn_connect should initiate");
        assert!(sys.has_provider("claude-code"));

        let second = sys.spawn_connect("claude-code", &mk_config());
        assert!(!second, "second spawn_connect on same agent should noop");
    }

    #[tokio::test]
    async fn forget_removes_provider() {
        let (mut sys, _rx) = mk_system();
        sys.spawn_connect("claude-code", &mk_config());
        assert!(sys.has_provider("claude-code"));
        sys.forget("claude-code");
        assert!(!sys.has_provider("claude-code"));
    }

    #[tokio::test]
    async fn shutdown_clears_all_providers() {
        let (mut sys, _rx) = mk_system();
        sys.spawn_connect("claude-code", &mk_config());
        sys.spawn_connect("codex", &mk_config());
        assert!(sys.has_provider("claude-code"));
        assert!(sys.has_provider("codex"));
        sys.shutdown();
        assert!(!sys.has_provider("claude-code"));
        assert!(!sys.has_provider("codex"));
    }

    #[tokio::test]
    async fn send_prompt_without_provider_is_noop() {
        let (sys, _rx) = mk_system();
        // Should not panic or block; just silently drop.
        sys.send_prompt("unknown", "session-id", "hello".to_owned());
    }

    #[tokio::test]
    async fn disconnect_unknown_agent_is_noop() {
        let (mut sys, _rx) = mk_system();
        sys.disconnect("unknown", "session-id");
        assert!(!sys.has_provider("unknown"));
    }
}
