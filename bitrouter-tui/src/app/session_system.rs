//! Session-lifecycle facade.
//!
//! Centralizes the four ACP lifecycle operations (connect, prompt,
//! disconnect, respond-permission). Routing is keyed by the local
//! [`SessionId`] so that multiple sessions on the same agent can run
//! independently — each one has its own ACP-assigned `acp_session_id`
//! and its own scrollback. The agent-level provider handle is shared
//! across all of an agent's sessions.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use bitrouter_config::AgentConfig;
use bitrouter_core::agents::event::{AgentEvent, PermissionRequestId, PermissionResponse};
use bitrouter_core::agents::provider::AgentProvider;
use bitrouter_providers::acp::provider::AcpAgentProvider;
use tokio::sync::mpsc;

use crate::event::AppEvent;
use crate::model::SessionId;

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

    /// The cwd recorded at startup, used when launching new ACP
    /// sessions or scanning agent storage for imports.
    pub fn launch_cwd(&self) -> &std::path::Path {
        &self.launch_cwd
    }

    /// Open a fresh ACP session against the agent's provider. Emits
    /// `SessionConnected` on success or a `Session` event with
    /// `AgentEvent::Error` on failure. The provider for `agent_id` is
    /// constructed lazily (one provider per agent, shared across that
    /// agent's sessions).
    pub fn spawn_session(&mut self, session_id: SessionId, agent_id: &str, config: &AgentConfig) {
        let provider = self
            .providers
            .entry(agent_id.to_string())
            .or_insert_with(|| {
                Arc::new(AcpAgentProvider::new(agent_id.to_string(), config.clone()))
            })
            .clone();

        let agent_id_owned = agent_id.to_string();
        let event_tx = self.event_tx.clone();
        let cwd = self.launch_cwd.clone();

        tokio::spawn(async move {
            match provider.connect(&cwd).await {
                Ok(info) => {
                    let _ = event_tx
                        .send(AppEvent::SessionConnected {
                            session_id,
                            agent_id: agent_id_owned,
                            acp_session_id: info.session_id,
                        })
                        .await;
                }
                Err(e) => {
                    let _ = event_tx
                        .send(AppEvent::Session {
                            session_id,
                            agent_id: agent_id_owned,
                            event: AgentEvent::Error {
                                message: format!("{e}"),
                            },
                        })
                        .await;
                }
            }
        });
    }

    /// Import an existing session by replaying its history via
    /// `session/load`. Like [`Self::spawn_session`] this is
    /// fire-and-forget — events stream back through the same
    /// `AppEvent::Session` channel as live prompts. The replay ends
    /// with a `HistoryReplayDone` event, after which the TUI can mark
    /// the session ready for new prompts.
    ///
    /// `external_id` is the agent-native session id (Claude `.jsonl`
    /// stem, Codex `payload.id`, etc.). Capability gating happens
    /// inside the provider — `load_session` errors out cleanly if the
    /// agent doesn't advertise `loadSession` in its initialize
    /// response.
    pub fn import_session(
        &mut self,
        session_id: SessionId,
        agent_id: &str,
        config: &AgentConfig,
        external_id: String,
    ) {
        let provider = self
            .providers
            .entry(agent_id.to_string())
            .or_insert_with(|| {
                Arc::new(AcpAgentProvider::new(agent_id.to_string(), config.clone()))
            })
            .clone();

        let agent_id_owned = agent_id.to_string();
        let event_tx = self.event_tx.clone();
        let cwd = self.launch_cwd.clone();

        tokio::spawn(async move {
            let load = provider.load_session(&cwd, &external_id).await;
            let (info, mut replay_rx) = match load {
                Ok(pair) => pair,
                Err(e) => {
                    let _ = event_tx
                        .send(AppEvent::Session {
                            session_id,
                            agent_id: agent_id_owned,
                            event: AgentEvent::Error {
                                message: format!("{e}"),
                            },
                        })
                        .await;
                    return;
                }
            };

            // Tell the TUI the ACP id is bound — same shape as the
            // post-`session/new` SessionConnected event.
            if event_tx
                .send(AppEvent::SessionConnected {
                    session_id,
                    agent_id: agent_id_owned.clone(),
                    acp_session_id: info.session_id,
                })
                .await
                .is_err()
            {
                return;
            }

            // Replay events until HistoryReplayDone (or the channel
            // closes for any reason — connection.rs always emits the
            // sentinel before closing the slot).
            while let Some(evt) = replay_rx.recv().await {
                if event_tx
                    .send(AppEvent::Session {
                        session_id,
                        agent_id: agent_id_owned.clone(),
                        event: evt,
                    })
                    .await
                    .is_err()
                {
                    break;
                }
            }
        });
    }

    /// Submit a prompt to an existing session and forward the turn's
    /// stream of `AgentEvent`s back, tagged with `session_id`.
    pub fn send_prompt(
        &self,
        session_id: SessionId,
        agent_id: &str,
        acp_session_id: &str,
        text: String,
    ) {
        let Some(provider) = self.providers.get(agent_id).cloned() else {
            return;
        };
        let agent_id_owned = agent_id.to_string();
        let acp_id_owned = acp_session_id.to_string();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            match provider.submit(&acp_id_owned, text).await {
                Ok(mut rx) => {
                    while let Some(evt) = rx.recv().await {
                        if event_tx
                            .send(AppEvent::Session {
                                session_id,
                                agent_id: agent_id_owned.clone(),
                                event: evt,
                            })
                            .await
                            .is_err()
                        {
                            break;
                        }
                    }
                }
                Err(e) => {
                    let _ = event_tx
                        .send(AppEvent::Session {
                            session_id,
                            agent_id: agent_id_owned,
                            event: AgentEvent::Error {
                                message: format!("{e}"),
                            },
                        })
                        .await;
                }
            }
        });
    }

    /// Tear down a single session. The provider stays alive for the
    /// agent's other sessions; it is only dropped on shutdown.
    pub fn disconnect_session(&self, agent_id: &str, acp_session_id: &str) {
        let Some(provider) = self.providers.get(agent_id).cloned() else {
            return;
        };
        let acp_id_owned = acp_session_id.to_string();
        tokio::spawn(async move {
            let _ = provider.disconnect(&acp_id_owned).await;
        });
    }

    /// Drop the provider handle for `agent_id` outright. Used after the
    /// agent reports a hard disconnect (process exit, crash) so we
    /// don't race a no-op `disconnect` on the way out.
    pub fn forget_provider(&mut self, agent_id: &str) {
        self.providers.remove(agent_id);
    }

    /// Resolve a pending permission request on the agent side.
    pub fn respond_permission(
        &self,
        agent_id: &str,
        acp_session_id: &str,
        request_id: PermissionRequestId,
        response: PermissionResponse,
    ) {
        let Some(provider) = self.providers.get(agent_id).cloned() else {
            return;
        };
        let acp_id_owned = acp_session_id.to_string();
        tokio::spawn(async move {
            let _ = provider
                .respond_permission(&acp_id_owned, request_id, response)
                .await;
        });
    }

    /// Drop all providers (shutdown path). Agent threads exit when
    /// their command-tx senders drop.
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
        // Tests don't actually spawn subprocesses, so any absolute
        // path works — picking `/` keeps the helper infallible.
        let cwd = std::path::PathBuf::from("/");
        (SessionSystem::new(tx, cwd), rx)
    }

    #[tokio::test]
    async fn has_provider_false_initially() {
        let (sys, _rx) = mk_system();
        assert!(!sys.has_provider("claude-code"));
    }

    #[tokio::test]
    async fn spawn_session_lazily_creates_provider() {
        let (mut sys, _rx) = mk_system();
        sys.spawn_session(SessionId(0), "claude-code", &mk_config());
        assert!(sys.has_provider("claude-code"));
    }

    #[tokio::test]
    async fn spawn_session_reuses_provider_for_same_agent() {
        let (mut sys, _rx) = mk_system();
        sys.spawn_session(SessionId(0), "claude-code", &mk_config());
        sys.spawn_session(SessionId(1), "claude-code", &mk_config());
        // Only one provider regardless of how many sessions point at it.
        assert_eq!(sys.providers.len(), 1);
    }

    #[tokio::test]
    async fn forget_provider_removes() {
        let (mut sys, _rx) = mk_system();
        sys.spawn_session(SessionId(0), "claude-code", &mk_config());
        assert!(sys.has_provider("claude-code"));
        sys.forget_provider("claude-code");
        assert!(!sys.has_provider("claude-code"));
    }

    #[tokio::test]
    async fn shutdown_clears_all_providers() {
        let (mut sys, _rx) = mk_system();
        sys.spawn_session(SessionId(0), "claude-code", &mk_config());
        sys.spawn_session(SessionId(1), "codex", &mk_config());
        sys.shutdown();
        assert!(!sys.has_provider("claude-code"));
        assert!(!sys.has_provider("codex"));
    }

    #[tokio::test]
    async fn send_prompt_without_provider_is_noop() {
        let (sys, _rx) = mk_system();
        sys.send_prompt(SessionId(0), "unknown", "acp-id", "hello".to_owned());
    }

    #[tokio::test]
    async fn disconnect_unknown_agent_is_noop() {
        let (sys, _rx) = mk_system();
        sys.disconnect_session("unknown", "acp-id");
    }
}
