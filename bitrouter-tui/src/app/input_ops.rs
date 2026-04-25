use crate::input;
use crate::model::{
    ActivityEntry, AgentStatus, AutocompleteState, EntryKind, InputTarget, ObsEvent, ObsEventKind,
    SessionStatus, UserPrompt, title_from_prompt,
};

use std::time::Instant;

use super::App;

impl App {
    pub(super) fn after_input_char(&mut self) {
        // Re-parse @-mentions to update the target indicator.
        let text = self.state.input.text();
        let agent_names: Vec<String> = self.state.agents.iter().map(|a| a.name.clone()).collect();
        self.state.input_target = input::parse_mentions(&text, &agent_names);
        self.update_autocomplete();
    }

    fn update_autocomplete(&mut self) {
        let (row, col) = self.state.input.cursor;
        let line = match self.state.input.lines.get(row) {
            Some(l) => l.as_str(),
            None => {
                self.state.autocomplete = None;
                return;
            }
        };

        let prefix = match input::autocomplete_prefix(line, col) {
            Some(p) => p,
            None => {
                self.state.autocomplete = None;
                return;
            }
        };

        let agent_names: Vec<String> = self.state.agents.iter().map(|a| a.name.clone()).collect();
        let candidates = input::filter_candidates(&prefix, &agent_names);
        if candidates.is_empty() {
            self.state.autocomplete = None;
        } else {
            self.state.autocomplete = Some(AutocompleteState {
                prefix,
                candidates,
                selected: 0,
            });
        }
    }

    pub(super) fn accept_autocomplete(&mut self) {
        let chosen = match &self.state.autocomplete {
            Some(ac) => ac.candidates.get(ac.selected).cloned(),
            None => None,
        };
        let prefix_len = self
            .state
            .autocomplete
            .as_ref()
            .map_or(0, |ac| ac.prefix.len());

        if let Some(name) = chosen {
            self.state.input.delete_before(prefix_len);
            self.state.input.insert_str(&name);
            self.state.input.insert_char(' ');
        }

        self.close_autocomplete();
        self.after_input_char();
    }

    pub(super) fn close_autocomplete(&mut self) {
        self.state.autocomplete = None;
    }

    pub(super) fn submit_input(&mut self) {
        let raw_text = self.state.input.text();
        if raw_text.trim().is_empty() {
            return;
        }

        // Slash commands take precedence over agent routing.  Unknown
        // `/...` input falls through to the prompt path so users can
        // still talk about literal slashes.
        if raw_text.trim_start().starts_with('/') {
            let cfg = self.bitrouter_config.clone();
            if self.try_handle_slash(&raw_text, &cfg) {
                self.state.input.clear();
                self.state.input_target = InputTarget::Default;
                self.close_autocomplete();
                return;
            }
        }

        let agent_names: Vec<String> = self.state.agents.iter().map(|a| a.name.clone()).collect();
        let target = input::parse_mentions(&raw_text, &agent_names);
        let clean_text = input::strip_mentions(&raw_text);

        if clean_text.trim().is_empty() {
            return;
        }

        // Resolve target agent(s).
        let targets: Vec<String> = match &target {
            InputTarget::Default => {
                // Route to active session's agent, or find first available.
                if let Some(name) = self.state.active_agent_name() {
                    vec![name.to_string()]
                } else {
                    // No active session — try first connected agent.
                    match self
                        .state
                        .agents
                        .iter()
                        .find(|a| matches!(a.status, AgentStatus::Connected | AgentStatus::Busy))
                    {
                        Some(a) => vec![a.name.clone()],
                        None => {
                            // Try first available agent (will lazy-connect).
                            match self.state.agents.iter().find(|a| a.config.is_some()) {
                                Some(a) => vec![a.name.clone()],
                                None => {
                                    self.push_system_msg("No agents available. Install an ACP agent and ensure it's on PATH.");
                                    return;
                                }
                            }
                        }
                    }
                }
            }
            InputTarget::Specific(names) => names.clone(),
            InputTarget::All => self
                .state
                .agents
                .iter()
                .filter(|a| {
                    matches!(
                        a.status,
                        AgentStatus::Connected
                            | AgentStatus::Busy
                            | AgentStatus::Idle
                            | AgentStatus::Available
                    ) && a.config.is_some()
                })
                .map(|a| a.name.clone())
                .collect(),
        };

        if targets.is_empty() {
            self.push_system_msg("No agents to send to.");
            return;
        }

        // For each target, find or create a session and push the user prompt
        // into that session's scrollback. Routing always goes through the
        // active session for `Default`, never an arbitrary first match.
        let active_session_id = self
            .state
            .session_store
            .active
            .get(self.state.active_session)
            .map(|s| s.id);
        let mut target_session_ids = Vec::with_capacity(targets.len());

        for (i, agent_name) in targets.iter().enumerate() {
            let session_idx = match (&target, i, active_session_id) {
                // For Default routing, the first target uses the active session.
                (InputTarget::Default, 0, Some(active_id))
                    if self
                        .state
                        .session_store
                        .index_of(active_id)
                        .map(|idx| self.state.session_store.active[idx].agent_id == *agent_name)
                        .unwrap_or(false) =>
                {
                    self.state
                        .session_store
                        .index_of(active_id)
                        .expect("active id checked above")
                }
                // Otherwise, find the first session for that agent or create one.
                _ => self.ensure_session_for_agent(agent_name),
            };
            let session_id = self.state.session_store.active[session_idx].id;
            target_session_ids.push((session_id, session_idx));

            // Auto-set the session's title from the first user prompt.
            let session = &mut self.state.session_store.active[session_idx];
            if session.title.is_none() {
                session.title = title_from_prompt(&clean_text);
            }

            let sb = &mut session.scrollback;
            let id = sb.next_id();
            sb.push_entry(ActivityEntry {
                id,
                kind: EntryKind::UserPrompt(UserPrompt {
                    text: raw_text.clone(),
                    targets: targets.clone(),
                }),
                collapsed: false,
            });
        }

        // Clear input.
        self.state.input.clear();
        self.state.input_target = InputTarget::Default;
        self.close_autocomplete();

        // Switch to the first target's session.
        if let Some(&(_, first_idx)) = target_session_ids.first() {
            self.switch_session(first_idx);
            if let Some(sb) = self.state.active_scrollback_mut() {
                sb.follow = true;
            }
        }

        // Send to each target session.
        for (i, agent_name) in targets.iter().enumerate() {
            let (session_id, session_idx) = target_session_ids[i];

            // If the session has no acp_session_id yet, kick off connect.
            let acp_id_present = self.state.session_store.active[session_idx]
                .acp_session_id
                .is_some();
            if !acp_id_present {
                if !self.session_system.has_provider(agent_name) {
                    // No provider yet: spawn one bound to this existing session.
                    let config = self
                        .state
                        .agents
                        .iter()
                        .find(|a| a.name == *agent_name)
                        .and_then(|a| a.config.clone());
                    if let Some(cfg) = config {
                        self.session_system
                            .spawn_session(session_id, agent_name, &cfg);
                    }
                }
                // Prompt will fail until SessionConnected lands; queue would be
                // a future enhancement. For now, surface a hint and skip.
                self.push_system_msg_to_session(
                    session_idx,
                    "Session still connecting — try again in a moment.",
                );
                continue;
            }

            // Reset streaming cursor on this session's scrollback for fresh response.
            let sb = &mut self.state.session_store.active[session_idx].scrollback;
            sb.streaming_entry.remove(agent_name);

            // Mark session + agent as busy.
            self.state.session_store.active[session_idx].status = SessionStatus::Busy;
            if let Some(agent) = self.state.agents.iter_mut().find(|a| &a.name == agent_name)
                && matches!(agent.status, AgentStatus::Connected)
            {
                agent.status = AgentStatus::Busy;
            }

            // Send prompt via provider (async via background task).
            self.send_prompt_to_session(session_id, clean_text.clone());
            self.state.obs_log.push(ObsEvent {
                agent_id: agent_name.clone(),
                kind: ObsEventKind::PromptSent,
                timestamp: Instant::now(),
            });
        }
    }
}
