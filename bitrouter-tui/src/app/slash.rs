//! Slash-command handling for the TUI input bar.
//!
//! When the user types a line starting with `/`, we try to parse it as
//! one of the supported commands and render its output into the active
//! tab's scrollback as system messages.  Unrecognized `/...` input falls
//! back to being sent to the agent as a prompt so users can still talk
//! about slashes.

use bitrouter_config::BitrouterConfig;
use bitrouter_config::acp::registry_agent_to_config;
use bitrouter_providers::acp::types::InstallProgress;
use tokio::sync::mpsc;

use crate::event::AppEvent;

use super::App;

/// Structured form of a recognised slash command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) enum SlashCommand {
    AgentsList {
        refresh: bool,
    },
    AgentsInstall {
        id: String,
    },
    AgentsUninstall {
        id: String,
    },
    AgentsUpdate {
        id: Option<String>,
    },
    AgentsDiscover,
    AgentsDisconnect {
        id: String,
    },
    ProvidersList,
    ProvidersUse {
        mode: String,
    },
    /// `/session` (or `/session list`) — list active and importable
    /// sessions inline.
    SessionList,
    /// `/session new [<agent>]` — spawn a new session, opening the
    /// agent picker when no argument is given.
    SessionNew {
        agent: Option<String>,
    },
    /// `/session switch [<id>]` — switch active session by id;
    /// opens picker when no argument.
    SessionSwitch {
        id: Option<u64>,
    },
    /// `/session close [<id>]` — close active or named session.
    SessionClose {
        id: Option<u64>,
    },
    /// `/session rename <new title>` — rename the active session's
    /// tab. The title can contain spaces.
    SessionRename {
        title: String,
    },
    /// `/session import [<agent> <external_id>]` — open multi-select
    /// picker, or import a specific session directly.
    SessionImport {
        target: Option<(String, String)>,
    },
    SessionPrev,
    SessionNext,
    SessionClear,
    /// Inline observability summary (replaces the v1 modal).
    Obs,
    Login,
    Logout,
    Whoami,
    Usage,
    KeysList,
    Init,
    Help,
}

/// Parse a `/`-prefixed input line.
///
/// Returns `None` if the line is not a slash command or is unrecognized
/// (the caller then decides whether to fall through to agent routing).
pub(super) fn parse_slash(line: &str) -> Option<SlashCommand> {
    let trimmed = line.trim();
    let rest = trimmed.strip_prefix('/')?;
    let mut parts = rest.split_whitespace();
    let head = parts.next()?;

    match head {
        "agents" => match parts.next() {
            None | Some("list") => {
                let refresh = parts.any(|p| p == "--refresh");
                Some(SlashCommand::AgentsList { refresh })
            }
            Some("install") => parts
                .next()
                .map(|id| SlashCommand::AgentsInstall { id: id.to_owned() }),
            Some("uninstall") => parts
                .next()
                .map(|id| SlashCommand::AgentsUninstall { id: id.to_owned() }),
            Some("update") => Some(SlashCommand::AgentsUpdate {
                id: parts.next().map(str::to_owned),
            }),
            Some("discover") => Some(SlashCommand::AgentsDiscover),
            Some("disconnect") => parts
                .next()
                .map(|id| SlashCommand::AgentsDisconnect { id: id.to_owned() }),
            _ => None,
        },
        "providers" => match parts.next() {
            None | Some("list") => Some(SlashCommand::ProvidersList),
            Some("use") => parts
                .next()
                .map(|m| SlashCommand::ProvidersUse { mode: m.to_owned() }),
            _ => None,
        },
        "session" => match parts.next() {
            None | Some("list") => Some(SlashCommand::SessionList),
            Some("new") => Some(SlashCommand::SessionNew {
                agent: parts.next().map(str::to_owned),
            }),
            Some("switch") => Some(SlashCommand::SessionSwitch {
                id: parts.next().and_then(|s| s.parse::<u64>().ok()),
            }),
            Some("close") => Some(SlashCommand::SessionClose {
                id: parts.next().and_then(|s| s.parse::<u64>().ok()),
            }),
            Some("rename") => {
                let rest: Vec<&str> = parts.collect();
                if rest.is_empty() {
                    None
                } else {
                    Some(SlashCommand::SessionRename {
                        title: rest.join(" "),
                    })
                }
            }
            Some("import") => {
                let agent = parts.next();
                let external = parts.next();
                let target = match (agent, external) {
                    (Some(a), Some(e)) => Some((a.to_owned(), e.to_owned())),
                    (None, None) => None,
                    _ => return None,
                };
                Some(SlashCommand::SessionImport { target })
            }
            Some("prev") => Some(SlashCommand::SessionPrev),
            Some("next") => Some(SlashCommand::SessionNext),
            Some("clear") => Some(SlashCommand::SessionClear),
            _ => None,
        },
        // Legacy alias — `/import` still works.
        "import" => {
            let agent = parts.next();
            let external = parts.next();
            let target = match (agent, external) {
                (Some(a), Some(e)) => Some((a.to_owned(), e.to_owned())),
                (None, None) => None,
                _ => return None,
            };
            Some(SlashCommand::SessionImport { target })
        }
        "obs" => Some(SlashCommand::Obs),
        "login" => Some(SlashCommand::Login),
        "logout" => Some(SlashCommand::Logout),
        "whoami" => Some(SlashCommand::Whoami),
        "usage" => Some(SlashCommand::Usage),
        "keys" => Some(SlashCommand::KeysList),
        "init" => Some(SlashCommand::Init),
        "help" | "?" => Some(SlashCommand::Help),
        _ => None,
    }
}

impl App {
    /// Handle a parsed slash command.  Returns `true` when the input
    /// was consumed (so the caller skips the agent-routing path).
    pub(super) fn try_handle_slash(
        &mut self,
        line: &str,
        bitrouter_config: &BitrouterConfig,
    ) -> bool {
        let Some(cmd) = parse_slash(line) else {
            return false;
        };

        match cmd {
            SlashCommand::AgentsList { refresh } => self.slash_agents_list(refresh),
            SlashCommand::AgentsInstall { id } => self.slash_agents_install(id, bitrouter_config),
            SlashCommand::AgentsUninstall { id } => self.slash_agents_uninstall(id),
            SlashCommand::AgentsUpdate { id } => self.slash_agents_update(id, bitrouter_config),
            SlashCommand::AgentsDiscover => {
                self.rediscover_agents();
                self.push_system_msg("Rescanned PATH for ACP agents.");
            }
            SlashCommand::AgentsDisconnect { id } => {
                self.disconnect_agent(&id);
                self.push_system_msg(&format!("Disconnecting all sessions for {id}..."));
            }
            SlashCommand::ProvidersList => self.slash_providers_list(bitrouter_config),
            SlashCommand::ProvidersUse { mode } => self.slash_providers_use(mode),
            SlashCommand::SessionList => self.slash_session_list(),
            SlashCommand::SessionNew { agent } => self.slash_session_new(agent),
            SlashCommand::SessionSwitch { id } => self.slash_session_switch(id),
            SlashCommand::SessionClose { id } => self.slash_session_close(id),
            SlashCommand::SessionRename { title } => self.slash_session_rename(title),
            SlashCommand::SessionImport { target } => self.slash_import(target),
            SlashCommand::SessionPrev => self.cycle_session_tab(false),
            SlashCommand::SessionNext => self.cycle_session_tab(true),
            SlashCommand::SessionClear => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    *sb = crate::model::ScrollbackState::new();
                }
            }
            SlashCommand::Obs => self.slash_obs(),
            SlashCommand::Login => self.push_system_msg(
                "Device-flow login runs in the CLI. Exit the TUI (Ctrl+Q) and run: bitrouter login",
            ),
            SlashCommand::Logout => {
                self.push_system_msg("Exit the TUI (Ctrl+Q) and run: bitrouter logout")
            }
            SlashCommand::Whoami => {
                self.push_system_msg("Exit the TUI (Ctrl+Q) and run: bitrouter whoami")
            }
            SlashCommand::Usage => self.push_system_msg(
                "Usage dashboard is not yet available in-TUI; tracked separately.",
            ),
            SlashCommand::KeysList => self.push_system_msg(
                "API keys are managed via `bitrouter auth set/remove` and ~/.bitrouter/.env",
            ),
            SlashCommand::Init => {
                self.push_system_msg("Re-run setup: exit the TUI (Ctrl+Q) and run `bitrouter init`")
            }
            SlashCommand::Help => self.run_help_command(),
        }
        true
    }

    fn slash_agents_list(&mut self, refresh: bool) {
        let cache_file = self.state.config.cache_dir.join("acp-registry.json");
        let state_file = self.state.config.agent_state_file.clone();
        let registry_override = self.bitrouter_config.acp_registry_url.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            use bitrouter_providers::acp::registry;
            use bitrouter_providers::acp::state;

            let url = registry::resolve_registry_url(registry_override.as_deref());
            let fetch = if refresh {
                registry::fetch_registry_fresh(&cache_file, &url).await
            } else {
                registry::fetch_registry(&cache_file, registry::DEFAULT_TTL_SECS, &url).await
            };

            let mut lines: Vec<String> = Vec::new();
            let records = state::load_state(&state_file).await.unwrap_or_default();

            match fetch {
                Ok(index) => {
                    lines.push(format!(
                        "Agents ({} in registry v{}):",
                        index.agents.len(),
                        index.version
                    ));
                    let mut agents = index.agents.clone();
                    agents.sort_by(|a, b| a.id.cmp(&b.id));
                    for a in &agents {
                        let installed = records.iter().find(|r| r.id == a.id);
                        let mark = match installed {
                            Some(r) => format!("[{}]", r.method),
                            None => "[ ]".to_owned(),
                        };
                        lines.push(format!("  {mark} {:<22} v{}", a.id, a.version));
                    }
                }
                Err(e) => lines.push(format!("Registry unavailable: {e}")),
            }

            send_system_lines(&event_tx, lines).await;
        });
    }

    fn slash_agents_install(&mut self, id: String, bitrouter_config: &BitrouterConfig) {
        self.push_system_msg(&format!("Installing {id}..."));

        let cache_file = self.state.config.cache_dir.join("acp-registry.json");
        let state_file = self.state.config.agent_state_file.clone();
        let install_dir = self.state.config.agents_dir.join(&id);
        let registry_override = bitrouter_config.acp_registry_url.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            use bitrouter_providers::acp::{eager, registry};

            let url = registry::resolve_registry_url(registry_override.as_deref());
            let index =
                match registry::fetch_registry(&cache_file, registry::DEFAULT_TTL_SECS, &url).await
                {
                    Ok(i) => i,
                    Err(e) => {
                        send_system_lines(&event_tx, vec![format!("Registry unavailable: {e}")])
                            .await;
                        return;
                    }
                };

            let Some(agent) = index.agents.iter().find(|a| a.id == id) else {
                send_system_lines(
                    &event_tx,
                    vec![format!("Agent '{id}' not found in registry")],
                )
                .await;
                return;
            };

            let agent_config = registry_agent_to_config(agent);
            let (tx, rx) = mpsc::channel(32);

            let reporter = spawn_progress_forwarder(rx, id.clone(), event_tx.clone());

            let result = eager::install_agent(
                &id,
                &agent_config,
                &install_dir,
                &state_file,
                &agent.version,
                tx,
            )
            .await;
            let _ = reporter.await;

            let line = match result {
                Ok(installed) => format!(
                    "✓ {} installed via {}",
                    installed.agent_id, installed.method
                ),
                Err(e) => format!("✗ install failed: {e}"),
            };
            send_system_lines(&event_tx, vec![line]).await;
        });
    }

    fn slash_agents_uninstall(&mut self, id: String) {
        let install_dir = self.state.config.agents_dir.join(&id);
        let state_file = self.state.config.agent_state_file.clone();
        let event_tx = self.event_tx.clone();

        tokio::spawn(async move {
            use bitrouter_providers::acp::eager;
            let line = match eager::uninstall_agent(&id, &install_dir, &state_file).await {
                Ok(()) => format!("✓ {id} uninstalled"),
                Err(e) => format!("✗ uninstall failed: {e}"),
            };
            send_system_lines(&event_tx, vec![line]).await;
        });
    }

    fn slash_agents_update(&mut self, id: Option<String>, bitrouter_config: &BitrouterConfig) {
        // `load_state_sync` is acceptable here because we're already on
        // the TUI's render/event-handling path (no active async ctx to
        // block) and the file is a small JSON array.
        let state_file = self.state.config.agent_state_file.clone();
        let records = bitrouter_providers::acp::state::load_state_sync(&state_file);
        if records.is_empty() {
            self.push_system_msg("(no agents installed)");
            return;
        }
        let targets: Vec<String> = match &id {
            Some(target) => {
                if !records.iter().any(|r| &r.id == target) {
                    self.push_system_msg(&format!("agent '{target}' is not installed"));
                    return;
                }
                vec![target.clone()]
            }
            None => records.iter().map(|r| r.id.clone()).collect(),
        };
        self.push_system_msg(&format!("Updating {} agent(s)...", targets.len()));
        for id in targets {
            self.slash_agents_install(id, bitrouter_config);
        }
    }

    fn slash_providers_list(&mut self, bitrouter_config: &BitrouterConfig) {
        if bitrouter_config.providers.is_empty() {
            self.push_system_msg("(no providers configured)");
            return;
        }
        let mut names: Vec<&String> = bitrouter_config.providers.keys().collect();
        names.sort();
        let mut lines = vec!["Providers:".to_owned()];
        for name in names {
            let p = &bitrouter_config.providers[name];
            let base = p.api_base.as_deref().unwrap_or("(derives)");
            let key = if p.api_key.is_some() {
                "✓ key set"
            } else if p.auth.is_some() {
                "✓ OAuth"
            } else {
                "✗ no creds"
            };
            lines.push(format!("  {name:<20} {base:<40} {key}"));
        }
        for line in lines {
            self.push_system_msg(&line);
        }
    }

    fn slash_import(&mut self, target: Option<(String, String)>) {
        // No-arg form: list discoverable sessions for the launch cwd.
        // Two-arg form: import the named session immediately.
        if let Some((agent_id, external_id)) = target {
            // Refuse a second import of the same external_session_id —
            // switch to the existing one instead so duplicate
            // /import calls are idempotent.
            if let Some(idx) = self.state.session_store.active.iter().position(|s| {
                s.agent_id == agent_id && s.external_session_id.as_deref() == Some(&external_id)
            }) {
                self.switch_session(idx);
                self.push_system_msg(&format!(
                    "Already imported {agent_id}/{external_id}; switched to it."
                ));
                return;
            }

            let home = match std::env::var_os("HOME").map(std::path::PathBuf::from) {
                Some(h) => h,
                None => {
                    self.push_system_msg("$HOME is unset; cannot resolve agent storage.");
                    return;
                }
            };
            let cwd = self.session_system.launch_cwd().to_path_buf();
            let scanned = bitrouter_providers::acp::session_import::scan_for_cwd(
                &home,
                &cwd,
                std::slice::from_ref(&agent_id),
            );
            let hit = scanned
                .iter()
                .find(|s| s.external_session_id == external_id);
            let (source_path, title_hint) = match hit {
                Some(s) => (s.source_path.clone(), s.title_hint.clone()),
                None => {
                    self.push_system_msg(&format!(
                        "No on-disk session '{external_id}' for {agent_id} in this cwd."
                    ));
                    return;
                }
            };
            if self
                .import_session(&agent_id, external_id.clone(), source_path, title_hint)
                .is_some()
            {
                self.push_system_msg(&format!("Importing {agent_id} session {external_id}..."));
            }
            return;
        }

        // No args: open the multi-select inline picker over discovered
        // on-disk sessions. We use the cached `state.discovered_sessions`
        // populated by the startup scan rather than re-scanning here —
        // freshness is fine within a single TUI session.
        if self.state.discovered_sessions.is_empty() {
            self.push_system_msg("No on-disk sessions found for this cwd.");
            return;
        }
        let candidates = self.state.discovered_sessions.clone();
        let entries = super::import_modal::build_import_entries(&candidates);

        // Flatten the grouped entries into picker rows. Group headers
        // are non-selectable; items are selectable. The picker action
        // payload only carries the selectable items, so we build a
        // parallel `selectables` vec and emit `PickerItem`s in the
        // same order, marking headers non-selectable.
        let mut items: Vec<crate::model::PickerItem> = Vec::with_capacity(entries.len());
        let mut selectables: Vec<crate::model::ImportCandidate> = Vec::new();
        for entry in entries {
            match entry {
                crate::model::ImportEntry::Group { agent_id, count } => {
                    items.push(crate::model::PickerItem {
                        label: format!("─── {agent_id} ({count})"),
                        subtitle: None,
                        selectable: false,
                    });
                }
                crate::model::ImportEntry::Item(c) => {
                    let label = c
                        .title_hint
                        .clone()
                        .unwrap_or_else(|| format!("(session {})", c.external_session_id));
                    items.push(crate::model::PickerItem {
                        label,
                        subtitle: Some(c.external_session_id.clone()),
                        selectable: true,
                    });
                    selectables.push(c);
                }
            }
        }

        // Picker indices are over the flat `items` list, but the
        // action carries only `selectables`. Map back: the picker
        // dispatcher receives indices into `items`; we need indices
        // into `selectables`. To bridge, we re-derive selectables
        // from the chosen indices in the dispatcher by skipping
        // non-Item rows. Easier: build a parallel selectable_index_at
        // vec mapping items[i] → Option<selectable_idx>.
        // For simplicity, we only allow selectable items via the
        // `selectable` flag; the picker dispatch resolves to indices
        // into `items`, and we filter to selectable items there.
        // To keep the action self-contained, expand `selectables`
        // such that `candidates[picker_index]` is the item at that
        // picker row when selectable, otherwise unused. Use a
        // sentinel by placing the same candidate at the matching
        // visual row; for header rows we insert a placeholder that
        // can never be selected (the picker filters non-selectable
        // rows from the cursor).
        let mut aligned: Vec<crate::model::ImportCandidate> = Vec::with_capacity(items.len());
        let mut sel_iter = selectables.into_iter();
        for it in &items {
            if it.selectable {
                if let Some(c) = sel_iter.next() {
                    aligned.push(c);
                } else {
                    // Should never happen — bail out cleanly.
                    aligned.push(placeholder_candidate());
                }
            } else {
                aligned.push(placeholder_candidate());
            }
        }

        self.open_picker(
            format!("Pick session(s) to import — {} found", candidates.len()),
            items,
            crate::model::PickerAction::Import {
                candidates: aligned,
            },
            true,
        );
    }

    /// Print inline help into the active session's scrollback.
    /// Replaces the v1 Help modal.
    pub(super) fn run_help_command(&mut self) {
        for line in HELP_TEXT {
            self.push_system_msg(line);
        }
    }

    /// `/session list` — print active sessions and any importable
    /// on-disk sessions for this cwd as system lines.
    fn slash_session_list(&mut self) {
        let mut lines: Vec<String> = Vec::new();

        if self.state.session_store.active.is_empty() {
            lines.push("(no active sessions)".to_string());
        } else {
            lines.push(format!(
                "Active sessions ({}):",
                self.state.session_store.active.len()
            ));
            for (i, s) in self.state.session_store.active.iter().enumerate() {
                let marker = if i == self.state.active_session {
                    "▸"
                } else {
                    " "
                };
                let label = s.title.clone().unwrap_or_else(|| s.agent_id.clone());
                let imported = match &s.source {
                    crate::model::SessionSource::Imported { .. } => "↓ ",
                    crate::model::SessionSource::Native => "  ",
                };
                lines.push(format!(
                    "  {marker} #{id}  {imported}{agent:<14}  {label}",
                    id = s.id.0,
                    agent = s.agent_id,
                ));
            }
        }

        if !self.state.discovered_sessions.is_empty() {
            lines.push(String::new());
            lines.push(format!(
                "Importable on-disk sessions ({}):",
                self.state.discovered_sessions.len()
            ));
            for c in self.state.discovered_sessions.iter().take(20) {
                let label = c.title_hint.as_deref().unwrap_or("(no title)");
                lines.push(format!(
                    "  /session import {agent} {id}  — {label}",
                    agent = c.agent_id,
                    id = c.external_session_id,
                ));
            }
            if self.state.discovered_sessions.len() > 20 {
                lines.push(format!(
                    "  …and {} more",
                    self.state.discovered_sessions.len() - 20
                ));
            }
        }

        for line in lines {
            self.push_system_msg(&line);
        }
    }

    /// Opens the same agent picker as `/session new` — used by the
    /// `+` tab-bar click in the top bar.
    pub(super) fn slash_session_new_via_mouse(&mut self) {
        self.slash_session_new(None);
    }

    /// `/session new [<agent>]` — pick an agent inline (or use the
    /// arg) and spawn a fresh session against it.
    fn slash_session_new(&mut self, agent: Option<String>) {
        if let Some(name) = agent {
            // Validate against the agent registry.
            if !self.state.agents.iter().any(|a| a.name == name) {
                self.push_system_msg(&format!("Unknown agent '{name}'. Try /agents."));
                return;
            }
            let len_before = self.state.session_store.active.len();
            self.connect_agent(&name);
            let len_after = self.state.session_store.active.len();
            if len_after > len_before {
                self.switch_session(len_after - 1);
            }
            return;
        }

        // No arg: open agent picker.
        let agents: Vec<String> = self.state.agents.iter().map(|a| a.name.clone()).collect();
        let items: Vec<crate::model::PickerItem> = self
            .state
            .agents
            .iter()
            .map(|a| {
                let subtitle = match &a.status {
                    crate::model::AgentStatus::Idle => "installed",
                    crate::model::AgentStatus::Available => "needs install",
                    crate::model::AgentStatus::Installing { .. } => "installing",
                    crate::model::AgentStatus::Connecting => "connecting",
                    crate::model::AgentStatus::Connected => "connected",
                    crate::model::AgentStatus::Busy => "busy",
                    crate::model::AgentStatus::Error(_) => "error",
                };
                crate::model::PickerItem {
                    label: a.name.clone(),
                    subtitle: Some(subtitle.to_string()),
                    selectable: true,
                }
            })
            .collect();
        self.open_picker(
            "Pick an agent for the new session".to_string(),
            items,
            crate::model::PickerAction::NewSession { agents },
            false,
        );
    }

    /// `/session switch [<id>]` — switch active session.
    fn slash_session_switch(&mut self, id: Option<u64>) {
        if let Some(target_id) = id {
            let sid = crate::model::SessionId(target_id);
            if let Some(idx) = self.state.session_store.index_of(sid) {
                self.switch_session(idx);
            } else {
                self.push_system_msg(&format!("No session with id #{target_id}."));
            }
            return;
        }
        // Picker.
        if self.state.session_store.active.is_empty() {
            self.push_system_msg("(no sessions to switch to)");
            return;
        }
        let ids: Vec<crate::model::SessionId> = self
            .state
            .session_store
            .active
            .iter()
            .map(|s| s.id)
            .collect();
        let items: Vec<crate::model::PickerItem> = self
            .state
            .session_store
            .active
            .iter()
            .map(|s| {
                let label = s.title.clone().unwrap_or_else(|| s.agent_id.clone());
                crate::model::PickerItem {
                    label: format!("#{}  {}", s.id.0, label),
                    subtitle: Some(s.agent_id.clone()),
                    selectable: true,
                }
            })
            .collect();
        self.open_picker(
            "Pick a session to switch to".to_string(),
            items,
            crate::model::PickerAction::SwitchSession { ids },
            false,
        );
    }

    /// `/session close [<id>]` — close the active or named session.
    fn slash_session_close(&mut self, id: Option<u64>) {
        if let Some(target_id) = id {
            let sid = crate::model::SessionId(target_id);
            let Some(idx) = self.state.session_store.index_of(sid) else {
                self.push_system_msg(&format!("No session with id #{target_id}."));
                return;
            };
            self.switch_session(idx);
        }
        self.close_current_session();
    }

    /// `/session rename <title>` — rename the active session.
    fn slash_session_rename(&mut self, title: String) {
        let idx = self.state.active_session;
        if let Some(s) = self.state.session_store.active.get_mut(idx) {
            s.title = Some(title);
        }
    }

    /// `/obs` — inline observability summary.
    fn slash_obs(&mut self) {
        let mut lines: Vec<String> = Vec::new();
        lines.push("Agents:".to_string());
        for a in &self.state.agents {
            let status = match &a.status {
                crate::model::AgentStatus::Idle => "idle".to_string(),
                crate::model::AgentStatus::Available => "available".to_string(),
                crate::model::AgentStatus::Installing { percent } => {
                    format!("installing {percent}%")
                }
                crate::model::AgentStatus::Connecting => "connecting".to_string(),
                crate::model::AgentStatus::Connected => "connected".to_string(),
                crate::model::AgentStatus::Busy => "busy".to_string(),
                crate::model::AgentStatus::Error(m) => format!("error: {m}"),
            };
            let session_count = self
                .state
                .session_store
                .active
                .iter()
                .filter(|s| s.agent_id == a.name)
                .count();
            let suffix = if session_count > 0 {
                format!("  ({session_count} session(s))")
            } else {
                String::new()
            };
            lines.push(format!("  {:<20} {}{}", a.name, status, suffix));
        }
        lines.push(String::new());
        lines.push(format!(
            "Recent events (last {}):",
            self.state.obs_log.events.len().min(50)
        ));
        for ev in self.state.obs_log.events.iter().rev().take(50) {
            let kind = match &ev.kind {
                crate::model::ObsEventKind::Connected => "connected".to_string(),
                crate::model::ObsEventKind::Disconnected => "disconnected".to_string(),
                crate::model::ObsEventKind::PromptSent => "prompt sent".to_string(),
                crate::model::ObsEventKind::PromptDone => "prompt done".to_string(),
                crate::model::ObsEventKind::ToolCall { title } => format!("tool: {title}"),
                crate::model::ObsEventKind::Error { message } => format!("error: {message}"),
            };
            let elapsed = ev.timestamp.elapsed();
            let when = if elapsed.as_secs() < 60 {
                format!("{}s ago", elapsed.as_secs())
            } else {
                format!("{}m ago", elapsed.as_secs() / 60)
            };
            lines.push(format!("  {when:>8}  [{}] {}", ev.agent_id, kind));
        }
        if self.state.obs_log.events.is_empty() {
            lines.push("  (no events yet)".to_string());
        }
        for line in lines {
            self.push_system_msg(&line);
        }
    }

    fn slash_providers_use(&mut self, mode: String) {
        let mode = mode.trim().to_lowercase();
        match mode.as_str() {
            "default" | "byok" => self.push_system_msg(&format!(
                "To switch to '{mode}', exit the TUI (Ctrl+Q) and run `bitrouter init`"
            )),
            other => self.push_system_msg(&format!(
                "unknown mode '{other}' (expected 'default' or 'byok')"
            )),
        }
    }
}

/// Sentinel ImportCandidate used as a placeholder for non-selectable
/// rows in the import picker's aligned action payload. Never visible
/// to the user — the picker's `selectable` flag prevents the cursor
/// from landing on these rows.
fn placeholder_candidate() -> crate::model::ImportCandidate {
    crate::model::ImportCandidate {
        agent_id: String::new(),
        external_session_id: String::new(),
        title_hint: None,
        last_active_at: 0,
        source_path: std::path::PathBuf::new(),
    }
}

// ── Help text ──────────────────────────────────────────────────────────

const HELP_TEXT: &[&str] = &[
    "BitRouter TUI — keyboard reference",
    "",
    "Normal mode (input bar):",
    "  Enter            send message (or run /<command>)",
    "  Shift+Enter      insert newline",
    "  Tab / Shift+Tab  next / previous session tab (or accept autocomplete)",
    "  ?  (empty input) run /help",
    "  Esc              enter Scroll mode",
    "  @<agent>         address a specific agent",
    "  Ctrl+W/U/K       delete word back / line start / line end",
    "  Ctrl+A / Ctrl+E  line start / end",
    "  Alt+← / Alt+→    word left / right",
    "  Ctrl+C           quit",
    "",
    "Scroll mode:",
    "  j/k or ↑/↓       scroll one line",
    "  PgUp/PgDn        scroll 20 lines",
    "  c                fold entry under cursor",
    "  /                search scrollback",
    "  G / i / printable return to input",
    "  Esc              return to input",
    "",
    "Permission mode (auto on incoming request):",
    "  y / n / a        allow once / deny / always allow",
    "",
    "Slash commands (type / for live autocomplete):",
    "  /session         list/new/switch/close/rename/import/clear active sessions",
    "  /agents          list/install/uninstall/update/discover/disconnect agents",
    "  /providers       list configured LLM providers",
    "  /obs             observability summary",
    "  /help or ?       this help",
];

/// Spawn a task that forwards [`InstallProgress`] events to the app's
/// [`AppEvent`] channel with real-percent computation.  Shared by the
/// slash-install path and (when invoked) the update path so progress
/// semantics stay consistent with `agent_lifecycle::start_binary_install`.
pub(super) fn spawn_progress_forwarder(
    mut rx: mpsc::Receiver<InstallProgress>,
    agent_id: String,
    tx: mpsc::Sender<AppEvent>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        while let Some(p) = rx.recv().await {
            let evt = match p {
                InstallProgress::Downloading {
                    bytes_received,
                    total,
                } => {
                    let percent = total
                        .filter(|&t| t > 0)
                        .map(|t| ((bytes_received * 100) / t) as u8)
                        .unwrap_or(0);
                    AppEvent::InstallProgress {
                        agent_id: agent_id.clone(),
                        percent,
                    }
                }
                InstallProgress::Extracting => AppEvent::InstallProgress {
                    agent_id: agent_id.clone(),
                    percent: 95,
                },
                InstallProgress::Done(path) => AppEvent::InstallComplete {
                    agent_id: agent_id.clone(),
                    binary_path: path,
                },
                InstallProgress::Failed(msg) => AppEvent::InstallFailed {
                    agent_id: agent_id.clone(),
                    message: msg,
                },
            };
            if tx.send(evt).await.is_err() {
                break;
            }
        }
    })
}

async fn send_system_lines(tx: &mpsc::Sender<AppEvent>, lines: Vec<String>) {
    for line in lines {
        if tx
            .send(AppEvent::SystemMessage { text: line })
            .await
            .is_err()
        {
            break;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_agents_list() {
        assert_eq!(
            parse_slash("/agents"),
            Some(SlashCommand::AgentsList { refresh: false })
        );
        assert_eq!(
            parse_slash("/agents list"),
            Some(SlashCommand::AgentsList { refresh: false })
        );
        assert_eq!(
            parse_slash("/agents list --refresh"),
            Some(SlashCommand::AgentsList { refresh: true })
        );
    }

    #[test]
    fn parses_agents_install_and_uninstall() {
        assert_eq!(
            parse_slash("/agents install claude-acp"),
            Some(SlashCommand::AgentsInstall {
                id: "claude-acp".to_owned()
            })
        );
        assert_eq!(
            parse_slash("/agents uninstall cline"),
            Some(SlashCommand::AgentsUninstall {
                id: "cline".to_owned()
            })
        );
        // Missing id → unrecognised.
        assert_eq!(parse_slash("/agents install"), None);
    }

    #[test]
    fn parses_agents_update_all_or_one() {
        assert_eq!(
            parse_slash("/agents update"),
            Some(SlashCommand::AgentsUpdate { id: None })
        );
        assert_eq!(
            parse_slash("/agents update codex-acp"),
            Some(SlashCommand::AgentsUpdate {
                id: Some("codex-acp".to_owned())
            })
        );
    }

    #[test]
    fn parses_providers_and_misc() {
        assert_eq!(parse_slash("/providers"), Some(SlashCommand::ProvidersList));
        assert_eq!(
            parse_slash("/providers use default"),
            Some(SlashCommand::ProvidersUse {
                mode: "default".to_owned()
            })
        );
        assert_eq!(parse_slash("/login"), Some(SlashCommand::Login));
        assert_eq!(parse_slash("/whoami"), Some(SlashCommand::Whoami));
        assert_eq!(parse_slash("/?"), Some(SlashCommand::Help));
    }

    #[test]
    fn non_slash_or_unknown_returns_none() {
        assert_eq!(parse_slash("hello world"), None);
        assert_eq!(parse_slash("/bogus"), None);
        assert_eq!(parse_slash(""), None);
    }

    #[test]
    fn parses_import_no_args_lists() {
        assert_eq!(
            parse_slash("/import"),
            Some(SlashCommand::SessionImport { target: None })
        );
        assert_eq!(
            parse_slash("/session import"),
            Some(SlashCommand::SessionImport { target: None })
        );
    }

    #[test]
    fn parses_import_with_agent_and_external_id() {
        assert_eq!(
            parse_slash("/import claude-code abc-123"),
            Some(SlashCommand::SessionImport {
                target: Some(("claude-code".to_owned(), "abc-123".to_owned()))
            })
        );
        assert_eq!(
            parse_slash("/session import claude-code abc-123"),
            Some(SlashCommand::SessionImport {
                target: Some(("claude-code".to_owned(), "abc-123".to_owned()))
            })
        );
    }

    #[test]
    fn parses_import_single_arg_is_rejected_as_ambiguous() {
        assert_eq!(parse_slash("/import claude-code"), None);
    }

    #[test]
    fn parses_session_subcommands() {
        assert_eq!(parse_slash("/session"), Some(SlashCommand::SessionList));
        assert_eq!(
            parse_slash("/session list"),
            Some(SlashCommand::SessionList)
        );
        assert_eq!(
            parse_slash("/session new"),
            Some(SlashCommand::SessionNew { agent: None })
        );
        assert_eq!(
            parse_slash("/session new claude-code"),
            Some(SlashCommand::SessionNew {
                agent: Some("claude-code".to_owned())
            })
        );
        assert_eq!(
            parse_slash("/session switch 3"),
            Some(SlashCommand::SessionSwitch { id: Some(3) })
        );
        assert_eq!(
            parse_slash("/session close"),
            Some(SlashCommand::SessionClose { id: None })
        );
        assert_eq!(
            parse_slash("/session rename refactor router"),
            Some(SlashCommand::SessionRename {
                title: "refactor router".to_owned()
            })
        );
        assert_eq!(
            parse_slash("/session prev"),
            Some(SlashCommand::SessionPrev)
        );
        assert_eq!(
            parse_slash("/session next"),
            Some(SlashCommand::SessionNext)
        );
        assert_eq!(
            parse_slash("/session clear"),
            Some(SlashCommand::SessionClear)
        );
    }

    #[test]
    fn parses_obs_and_agents_meta() {
        assert_eq!(parse_slash("/obs"), Some(SlashCommand::Obs));
        assert_eq!(
            parse_slash("/agents discover"),
            Some(SlashCommand::AgentsDiscover)
        );
        assert_eq!(
            parse_slash("/agents disconnect claude-code"),
            Some(SlashCommand::AgentsDisconnect {
                id: "claude-code".to_owned()
            })
        );
    }
}
