use bitrouter_providers::acp::discovery::discover_agents;
use bitrouter_providers::acp::provider::AcpAgentProvider;
use bitrouter_providers::acp::types::{
    AgentAvailability, AgentEvent, PermissionOutcome, PermissionResponse,
};
use tokio::sync::mpsc;

use crate::event::AppEvent;
use crate::model::{ActivityEntry, AgentStatus, EntryKind, PermissionEntry, TabBadge, agent_color};

use super::helpers::{PermissionChoice, needs_binary_install};
use super::{App, InputMode};

impl App {
    pub(super) fn connect_agent(&mut self, agent_id: &str) {
        if self.agent_providers.contains_key(agent_id) {
            return; // Already connected or connecting.
        }

        let agent = match self.state.agents.iter_mut().find(|a| a.name == agent_id) {
            Some(a) => a,
            None => return,
        };

        // Don't interrupt an install or connection already in progress.
        if matches!(
            agent.status,
            AgentStatus::Connecting | AgentStatus::Installing { .. }
        ) {
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
            self.ensure_tab(agent_id);
            self.push_system_msg(&format!("Installing {agent_id}..."));
            self.start_binary_install(agent_id, &config);
            return;
        }

        agent.status = AgentStatus::Connecting;

        // Ensure a tab exists for this agent.
        self.ensure_tab(agent_id);

        self.spawn_agent_provider(agent_id, &config);
    }

    /// Spawn the async binary download task.
    fn start_binary_install(&self, agent_id: &str, config: &bitrouter_config::AgentConfig) {
        use bitrouter_config::Distribution;
        use bitrouter_providers::acp::install::install_binary_agent;

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

        tokio::spawn(async move {
            let (progress_tx, mut progress_rx) = mpsc::channel(32);

            // Forward progress to app events.
            let fwd_tx = event_tx.clone();
            let fwd_id = agent_id_owned.clone();
            tokio::spawn(async move {
                while let Some(p) = progress_rx.recv().await {
                    use bitrouter_providers::acp::types::InstallProgress;
                    let evt = match &p {
                        InstallProgress::Downloading {
                            bytes_received,
                            total,
                        } => {
                            let percent = total
                                .filter(|&t| t > 0)
                                .map(|t| ((*bytes_received * 100) / t) as u8)
                                .unwrap_or(0);
                            AppEvent::InstallProgress {
                                agent_id: fwd_id.clone(),
                                percent,
                            }
                        }
                        InstallProgress::Extracting => AppEvent::InstallProgress {
                            agent_id: fwd_id.clone(),
                            percent: 95,
                        },
                        InstallProgress::Done(path) => AppEvent::InstallComplete {
                            agent_id: fwd_id.clone(),
                            binary_path: path.clone(),
                        },
                        InstallProgress::Failed(msg) => AppEvent::InstallFailed {
                            agent_id: fwd_id.clone(),
                            message: msg.clone(),
                        },
                    };
                    if fwd_tx.send(evt).await.is_err() {
                        break;
                    }
                }
            });

            // The forwarding task handles Done/Failed via InstallProgress,
            // so we only need to drive the install to completion here.
            let _ = install_binary_agent(&agent_id_owned, &platforms, progress_tx).await;
        });
    }

    /// Handle a completed binary install by spawning the agent connection.
    pub(super) fn handle_install_complete(
        &mut self,
        agent_id: &str,
        binary_path: std::path::PathBuf,
    ) {
        let agent = match self.state.agents.iter_mut().find(|a| a.name == *agent_id) {
            Some(a) => a,
            None => return,
        };

        // Update config to use the installed binary path and archive args.
        if let Some(config) = &mut agent.config {
            config.binary = binary_path.to_string_lossy().into_owned();
            // If the binary archive specifies args, use those.
            if let Some(archive_args) = Self::binary_archive_args(config) {
                config.args = archive_args;
            }
        }

        let config = match &agent.config {
            Some(c) => c.clone(),
            None => return,
        };

        agent.status = AgentStatus::Connecting;
        self.push_system_msg(&format!("{agent_id} installed, connecting..."));
        self.spawn_agent_provider(agent_id, &config);
    }

    /// Extract args from the binary archive matching the current platform.
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

    /// Spawn an ACP agent provider and wire up event forwarding.
    fn spawn_agent_provider(&mut self, agent_id: &str, config: &bitrouter_config::AgentConfig) {
        let (agent_event_tx, mut agent_event_rx) = mpsc::channel::<AgentEvent>(256);
        let app_event_tx = self.event_tx.clone();
        tokio::spawn(async move {
            while let Some(evt) = agent_event_rx.recv().await {
                if app_event_tx.send(AppEvent::Agent(evt)).await.is_err() {
                    break;
                }
            }
        });

        let provider = AcpAgentProvider::spawn(agent_id.to_string(), config, agent_event_tx);
        self.agent_providers.insert(agent_id.to_string(), provider);
    }

    pub(super) fn disconnect_agent(&mut self, agent_id: &str) {
        // Drop the provider (closes command channel → agent thread exits).
        self.agent_providers.remove(agent_id);
        // The agent thread will send AgentDisconnected, which handles the rest.
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
                // Update status for idle/available agents (don't touch connected ones).
                if matches!(
                    agent.status,
                    AgentStatus::Idle | AgentStatus::Available | AgentStatus::Error(_)
                ) {
                    agent.status = new_status;
                }
            } else {
                // New agent not yet in list.
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
                    }),
                    status: new_status,
                    session_id: None,
                    color: agent_color(idx),
                });
            }
        }
    }

    pub(super) fn handle_permission_request(
        &mut self,
        agent_id: String,
        request: bitrouter_providers::acp::types::PermissionRequest,
        response_tx: tokio::sync::oneshot::Sender<PermissionResponse>,
    ) {
        let tab_idx = self.ensure_tab(&agent_id);
        let sb = &mut self.state.tabs[tab_idx].scrollback;

        let id = sb.next_id();
        sb.push_entry(ActivityEntry {
            id,
            kind: EntryKind::Permission(PermissionEntry {
                agent_id: agent_id.clone(),
                request: Box::new(request),
                response_tx: Some(response_tx),
                resolved: false,
            }),
            collapsed: false,
        });
        // Re-pin to bottom so user sees the permission prompt.
        sb.follow = true;

        // Auto-switch only if we're not already resolving a permission on another tab.
        if self.state.mode == InputMode::Permission {
            // Already handling a permission — just badge this tab, don't switch.
            if tab_idx != self.state.active_tab {
                self.state.tabs[tab_idx].badge = TabBadge::Permission;
            }
        } else {
            if tab_idx != self.state.active_tab {
                self.state.tabs[tab_idx].badge = TabBadge::Permission;
                self.switch_tab(tab_idx);
            }
            self.state.mode = InputMode::Permission;
        }
    }

    pub(super) fn resolve_permission(&mut self, entry_idx: usize, choice: PermissionChoice) {
        let sb = match self.state.active_scrollback_mut() {
            Some(sb) => sb,
            None => return,
        };

        if let EntryKind::Permission(perm) = &mut sb.entries[entry_idx].kind {
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
                    // Pick the "always" option if it exists, else first option.
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

            if let Some(tx) = perm.response_tx.take() {
                let _ = tx.send(PermissionResponse { outcome });
            }
            perm.resolved = true;
        }

        // Check if any other tab has a pending permission — auto-switch to it.
        let next_perm_tab = self.state.tabs.iter().enumerate().find(|(_, tab)| {
            tab.scrollback
                .entries
                .iter()
                .any(|e| matches!(&e.kind, EntryKind::Permission(p) if !p.resolved))
        });
        if let Some((idx, _)) = next_perm_tab {
            self.switch_tab(idx);
            self.state.mode = InputMode::Permission;
        } else {
            self.state.mode = InputMode::Normal;
        }
    }
}
