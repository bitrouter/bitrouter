use crate::input;
use crate::model::{
    ActivityEntry, AgentStatus, AutocompleteState, EntryKind, InputTarget, ObsEvent, ObsEventKind,
    UserPrompt,
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
                // Route to active tab's agent, or find first available.
                if let Some(name) = self.state.active_agent_name() {
                    vec![name.to_string()]
                } else {
                    // No active tab — try first connected agent.
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

        // Push user prompt to each target tab's scrollback.
        for agent_name in &targets {
            let tab_idx = self.ensure_tab(agent_name);
            let sb = &mut self.state.tabs[tab_idx].scrollback;
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

        // Switch to the first target's tab.
        if let Some(first_target) = targets.first()
            && let Some(tab_idx) = self.tab_for_agent(first_target)
        {
            self.switch_tab(tab_idx);
            if let Some(sb) = self.state.active_scrollback_mut() {
                sb.follow = true;
            }
        }

        // Send to each target agent.
        for agent_name in &targets {
            // Lazy-connect if needed.
            if !self.agent_providers.contains_key(agent_name) {
                self.connect_agent(agent_name);
            }
            // Reset streaming cursor for fresh response.
            if let Some(sb) = self.scrollback_for_agent(agent_name) {
                sb.streaming_entry.remove(agent_name);
            }

            // Mark as busy.
            if let Some(agent) = self.state.agents.iter_mut().find(|a| &a.name == agent_name)
                && matches!(agent.status, AgentStatus::Connected)
            {
                agent.status = AgentStatus::Busy;
            }

            // Send prompt via provider (async via background task).
            self.send_prompt_to_agent(agent_name, clean_text.clone());
            self.state.obs_log.push(ObsEvent {
                agent_id: agent_name.clone(),
                kind: ObsEventKind::PromptSent,
                timestamp: Instant::now(),
            });
        }
    }
}
