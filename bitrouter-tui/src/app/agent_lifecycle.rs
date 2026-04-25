use bitrouter_core::agents::event::{
    PermissionOutcome, PermissionRequest, PermissionRequestId, PermissionResponse,
};
use bitrouter_providers::acp::discovery::discover_agents;
use bitrouter_providers::acp::types::AgentAvailability;
use tokio::sync::mpsc;

use crate::event::AppEvent;
use crate::model::{
    ActivityEntry, AgentStatus, EntryKind, PermissionEntry, SessionBadge, SessionId, SessionStatus,
    agent_color,
};

use super::helpers::{PermissionChoice, needs_binary_install};
use super::{App, InputMode};

impl App {
    /// User chose an agent to spin up. Always creates a fresh session
    /// (multiple sessions per agent are allowed). Reuses the existing
    /// provider if one is already live.
    pub(super) fn connect_agent(&mut self, agent_id: &str) {
        let agent = match self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            Some(a) => a,
            None => return,
        };

        // Don't kick off a fresh install/connect for the agent itself
        // while one is already in progress (any session would inherit
        // the same provider once it lands).
        if matches!(agent.status, AgentStatus::Installing { .. }) {
            return;
        }

        let config = match &agent.config {
            Some(c) => c.clone(),
            None => {
                let msg = format!(
                    "No ACP adapter found for {agent_id}. Install the adapter and ensure it's on PATH."
                );
                self.push_system_msg(&msg);
                return;
            }
        };

        // Binary-only distribution: need to download first.
        if agent.status == AgentStatus::Available && needs_binary_install(&config) {
            agent.status = AgentStatus::Installing { percent: 0 };
            self.create_session_for_agent(agent_id);
            self.push_system_msg(&format!("Installing {agent_id}..."));
            self.start_binary_install(agent_id, &config);
            return;
        }

        agent.status = AgentStatus::Connecting;

        let session_idx = self.create_session_for_agent(agent_id);
        let session_id = self.state.session_store.active[session_idx].id;
        self.session_system
            .spawn_session(session_id, agent_id, &config);
    }

    /// Import an existing on-disk session via `session/load`. Creates
    /// a fresh `Session` entry tagged as imported, then dispatches the
    /// replay through `SessionSystem::import_session`. The session
    /// stays in `Connecting` until the replay's `HistoryReplayDone`
    /// event fences the imported history with a separator.
    pub(super) fn import_session(
        &mut self,
        agent_id: &str,
        external_session_id: String,
        source_path: std::path::PathBuf,
        title_hint: Option<String>,
    ) -> Option<SessionId> {
        let agent = self.state.agents.iter_mut().find(|a| a.name == agent_id)?;
        if matches!(agent.status, AgentStatus::Installing { .. }) {
            self.push_system_msg(&format!("{agent_id} is installing — try again shortly."));
            return None;
        }
        let config = match &agent.config {
            Some(c) => c.clone(),
            None => {
                self.push_system_msg(&format!("No ACP adapter configured for {agent_id}."));
                return None;
            }
        };
        agent.status = AgentStatus::Connecting;

        let session_idx = self.create_imported_session(
            agent_id,
            external_session_id.clone(),
            source_path,
            title_hint,
        );
        let session_id = self.state.session_store.active[session_idx].id;
        self.session_system
            .import_session(session_id, agent_id, &config, external_session_id);
        Some(session_id)
    }

    /// Spawn the async binary download task (click-connect path).
    fn start_binary_install(&self, agent_id: &str, config: &bitrouter_config::AgentConfig) {
        use bitrouter_config::Distribution;
        use bitrouter_providers::acp::install::install_binary_agent;
        use bitrouter_providers::acp::state::{
            InstallMethod, InstallRecord, now_unix_seconds, upsert_record,
        };

        let platforms = config.distribution.iter().find_map(|d| match d {
            Distribution::Binary { platforms } => Some(platforms.clone()),
            _ => None,
        });

        let platforms = match platforms {
            Some(p) => p,
            None => return,
        };

        let agent_id_owned = agent_id.to_string();
        let event_tx = self.event_tx.clone();
        let install_dir = self.state.config.agents_dir.join(&agent_id_owned);
        let state_file = self.state.config.agent_state_file.clone();

        tokio::spawn(async move {
            let (progress_tx, progress_rx) = mpsc::channel(32);
            let reporter = super::slash::spawn_progress_forwarder(
                progress_rx,
                agent_id_owned.clone(),
                event_tx.clone(),
            );

            let result =
                install_binary_agent(&agent_id_owned, &install_dir, &platforms, progress_tx).await;
            let _ = reporter.await;

            match result {
                Ok(path) => {
                    let record = InstallRecord {
                        id: agent_id_owned,
                        version: String::new(),
                        method: InstallMethod::Binary,
                        resolved_binary_path: Some(path),
                        installed_at: now_unix_seconds(),
                    };
                    let _ = upsert_record(&state_file, record).await;
                }
                Err(e) => {
                    // Surface the failure so the placeholder session
                    // doesn't leak in `Connecting` and the agent
                    // doesn't stay in `Installing` forever.
                    let _ = event_tx
                        .send(AppEvent::InstallFailed {
                            agent_id: agent_id_owned,
                            message: e.to_string(),
                        })
                        .await;
                }
            }
        });
    }

    /// Handle a completed binary install by spawning the connection on
    /// the most-recent waiting session (the one created in
    /// `connect_agent` above).
    pub(super) fn handle_install_complete(
        &mut self,
        agent_id: &str,
        binary_path: std::path::PathBuf,
    ) {
        let agent = match self.state.agents.iter_mut().find(|a| a.name == *agent_id) {
            Some(a) => a,
            None => return,
        };

        if let Some(config) = &mut agent.config {
            config.binary = binary_path.to_string_lossy().into_owned();
            if let Some(archive_args) = Self::binary_archive_args(config) {
                config.args = archive_args;
            }
        }

        let config = match &agent.config {
            Some(c) => c.clone(),
            None => return,
        };

        let _ = binary_path;

        agent.status = AgentStatus::Connecting;
        self.push_system_msg(&format!("{agent_id} installed, connecting..."));

        // Re-use the placeholder session created in connect_agent if
        // it still exists; otherwise allocate a fresh one.
        let session_idx = self
            .state
            .session_store
            .active
            .iter()
            .position(|s| s.agent_id == agent_id && s.acp_session_id.is_none())
            .unwrap_or_else(|| self.create_session_for_agent(agent_id));
        let session_id = self.state.session_store.active[session_idx].id;
        self.session_system
            .spawn_session(session_id, agent_id, &config);
    }

    fn binary_archive_args(config: &bitrouter_config::AgentConfig) -> Option<Vec<String>> {
        use bitrouter_config::Distribution;
        use bitrouter_providers::acp::platform::current_platform;

        let platform = current_platform()?;
        for dist in &config.distribution {
            if let Distribution::Binary { platforms } = dist
                && let Some(archive) = platforms.get(platform)
                && !archive.args.is_empty()
            {
                return Some(archive.args.clone());
            }
        }
        None
    }

    /// Send a prompt to a specific session.
    pub(super) fn send_prompt_to_session(&self, session_id: SessionId, text: String) {
        let Some(idx) = self.state.session_store.index_of(session_id) else {
            return;
        };
        let session = &self.state.session_store.active[idx];
        let Some(acp_id) = session.acp_session_id.as_deref() else {
            return; // session not yet connected
        };
        self.session_system
            .send_prompt(session_id, &session.agent_id, acp_id, text);
    }

    /// Disconnect every session bound to `agent_id`.
    pub(super) fn disconnect_agent(&mut self, agent_id: &str) {
        let to_close: Vec<String> = self
            .state
            .session_store
            .active
            .iter()
            .filter(|s| s.agent_id == agent_id)
            .filter_map(|s| s.acp_session_id.clone())
            .collect();
        for acp_id in to_close {
            self.session_system.disconnect_session(agent_id, &acp_id);
        }
        // The disconnect will trigger a Disconnected event from the agent thread.
    }

    pub(super) fn rediscover_agents(&mut self) {
        let known = bitrouter_config::builtin_agent_defs();
        let discovered = discover_agents(&known);

        for da in &discovered {
            let new_status = match &da.availability {
                AgentAvailability::OnPath(_) => AgentStatus::Idle,
                AgentAvailability::Distributable => AgentStatus::Available,
            };

            if let Some(agent) = self.state.agents.iter_mut().find(|a| a.name == da.name) {
                if matches!(
                    agent.status,
                    AgentStatus::Idle | AgentStatus::Available | AgentStatus::Error(_)
                ) {
                    agent.status = new_status;
                }
            } else {
                let idx = self.state.agents.len();
                let distribution = known
                    .get(&da.name)
                    .map(|c| c.distribution.clone())
                    .unwrap_or_default();
                self.state.agents.push(crate::model::Agent {
                    name: da.name.clone(),
                    config: Some(bitrouter_config::AgentConfig {
                        protocol: bitrouter_config::AgentProtocol::Acp,
                        binary: da.binary.to_string_lossy().into_owned(),
                        args: da.args.clone(),
                        enabled: true,
                        distribution,
                        session: None,
                        a2a: None,
                    }),
                    status: new_status,
                    color: agent_color(idx),
                });
            }
        }
    }

    pub(super) fn handle_permission_request(
        &mut self,
        session_id: SessionId,
        request_id: PermissionRequestId,
        request: PermissionRequest,
    ) {
        let Some(session_idx) = self.state.session_store.index_of(session_id) else {
            return;
        };
        let agent_id = self.state.session_store.active[session_idx]
            .agent_id
            .clone();
        let sb = &mut self.state.session_store.active[session_idx].scrollback;

        let id = sb.next_id();
        sb.push_entry(ActivityEntry {
            id,
            kind: EntryKind::Permission(PermissionEntry {
                agent_id: agent_id.clone(),
                request_id,
                request: Box::new(request),
                resolved: false,
            }),
            collapsed: false,
        });
        sb.follow = true;

        if self.state.mode == InputMode::Permission {
            if session_idx != self.state.active_session {
                self.state.session_store.active[session_idx].badge = SessionBadge::Permission;
            }
        } else {
            if session_idx != self.state.active_session {
                self.state.session_store.active[session_idx].badge = SessionBadge::Permission;
                self.switch_session(session_idx);
            }
            self.state.mode = InputMode::Permission;
        }
    }

    pub(super) fn resolve_permission(&mut self, entry_idx: usize, choice: PermissionChoice) {
        let active_idx = self.state.active_session;
        let Some(session) = self.state.session_store.active.get_mut(active_idx) else {
            return;
        };
        let agent_id = session.agent_id.clone();
        let acp_session_id = session.acp_session_id.clone();
        let sb = &mut session.scrollback;

        // Compute outcome WITHOUT marking the entry resolved yet — we
        // only commit if we can actually deliver the response.
        let (request_id, outcome) = if let EntryKind::Permission(perm) = &sb.entries[entry_idx].kind
        {
            let outcome = match choice {
                PermissionChoice::Yes => {
                    if let Some(opt) = perm.request.options.first() {
                        PermissionOutcome::Allowed {
                            selected_option: opt.id.clone(),
                        }
                    } else {
                        PermissionOutcome::Denied
                    }
                }
                PermissionChoice::Always => {
                    let always_opt = perm
                        .request
                        .options
                        .iter()
                        .find(|o| o.id.to_lowercase().contains("always"));
                    if let Some(opt) = always_opt.or(perm.request.options.first()) {
                        PermissionOutcome::Allowed {
                            selected_option: opt.id.clone(),
                        }
                    } else {
                        PermissionOutcome::Denied
                    }
                }
                PermissionChoice::No => PermissionOutcome::Denied,
            };

            (perm.request_id, outcome)
        } else {
            return;
        };

        let Some(acp_id) = acp_session_id else {
            // Session not yet connected — can't deliver the response.
            // Leave the entry unresolved so the user can retry once
            // SessionConnected lands.
            self.push_system_msg_to_session(
                active_idx,
                "Cannot respond: session not yet connected — try again in a moment.",
            );
            return;
        };

        // Mark resolved and dispatch.
        if let Some(perm_session) = self.state.session_store.active.get_mut(active_idx)
            && let EntryKind::Permission(perm) =
                &mut perm_session.scrollback.entries[entry_idx].kind
        {
            perm.resolved = true;
        }
        self.session_system.respond_permission(
            &agent_id,
            &acp_id,
            request_id,
            PermissionResponse { outcome },
        );

        // Check if any other session has a pending permission — auto-switch to it.
        let next_perm_session =
            self.state
                .session_store
                .active
                .iter()
                .enumerate()
                .find(|(_, session)| {
                    session
                        .scrollback
                        .entries
                        .iter()
                        .any(|e| matches!(&e.kind, EntryKind::Permission(p) if !p.resolved))
                });
        if let Some((idx, _)) = next_perm_session {
            self.switch_session(idx);
            self.state.mode = InputMode::Permission;
        } else {
            self.state.mode = InputMode::Normal;
        }
    }

    /// Internal helper: set the per-session status.
    pub(super) fn set_session_status(&mut self, session_id: SessionId, status: SessionStatus) {
        if let Some(idx) = self.state.session_store.index_of(session_id) {
            self.state.session_store.active[idx].status = status;
        }
    }
}
