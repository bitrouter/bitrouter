use crossterm::event::{KeyCode, KeyEvent};

use crate::model::{
    AgentStatus, CommandAction, CommandPaletteState, ImportEntry, Modal, ObservabilityState,
    PaletteCommand, ScrollbackState,
};

use super::App;

impl App {
    pub(super) fn handle_modal_key(&mut self, key: KeyEvent) {
        let modal_kind = match &self.state.modal {
            Some(Modal::Observability(_)) => 0,
            Some(Modal::CommandPalette(_)) => 1,
            Some(Modal::Help) => 2,
            Some(Modal::ImportThreads(_)) => 3,
            None => return,
        };

        match modal_kind {
            0 => self.handle_observability_key(key),
            1 => self.handle_command_palette_key(key),
            2 if (key.code == KeyCode::Esc || key.code == KeyCode::Char('?')) => {
                self.state.modal = None;
            }
            3 => self.handle_import_modal_key(key),
            _ => {}
        }
    }

    fn handle_import_modal_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Esc => {
                self.dismiss_import_modal();
            }
            KeyCode::Enter => {
                self.confirm_import_modal();
            }
            KeyCode::Char(' ') => {
                self.toggle_import_selection_at_cursor();
            }
            KeyCode::Down | KeyCode::Char('j') => {
                self.move_import_cursor(1);
            }
            KeyCode::Up | KeyCode::Char('k') => {
                self.move_import_cursor(-1);
            }
            // 'a' selects every visible item; 'n' clears selections.
            KeyCode::Char('a') => self.select_all_import_items(),
            KeyCode::Char('n') => self.clear_import_selections(),
            _ => {}
        }
    }

    fn toggle_import_selection_at_cursor(&mut self) {
        let Some(Modal::ImportThreads(state)) = self.state.modal.as_mut() else {
            return;
        };
        let idx = state.cursor;
        if !matches!(state.entries.get(idx), Some(ImportEntry::Item(_))) {
            return;
        }
        if !state.selected.insert(idx) {
            state.selected.remove(&idx);
        }
    }

    fn move_import_cursor(&mut self, delta: i32) {
        let Some(Modal::ImportThreads(state)) = self.state.modal.as_mut() else {
            return;
        };
        if state.entries.is_empty() {
            return;
        }
        let len = state.entries.len() as i32;
        let mut next = state.cursor as i32;
        // Step until we land on a selectable item or wrap back to start.
        for _ in 0..len {
            next = (next + delta).rem_euclid(len);
            if matches!(state.entries.get(next as usize), Some(ImportEntry::Item(_))) {
                state.cursor = next as usize;
                return;
            }
        }
    }

    fn select_all_import_items(&mut self) {
        let Some(Modal::ImportThreads(state)) = self.state.modal.as_mut() else {
            return;
        };
        for (idx, entry) in state.entries.iter().enumerate() {
            if matches!(entry, ImportEntry::Item(_)) {
                state.selected.insert(idx);
            }
        }
    }

    fn clear_import_selections(&mut self) {
        let Some(Modal::ImportThreads(state)) = self.state.modal.as_mut() else {
            return;
        };
        state.selected.clear();
    }

    fn handle_observability_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down | KeyCode::Char('j') => {
                if let Some(Modal::Observability(s)) = &mut self.state.modal {
                    s.scroll_offset = s.scroll_offset.saturating_add(1);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if let Some(Modal::Observability(s)) = &mut self.state.modal {
                    s.scroll_offset = s.scroll_offset.saturating_sub(1);
                }
            }
            KeyCode::Esc => {
                self.state.modal = None;
            }
            _ => {}
        }
    }

    fn handle_command_palette_key(&mut self, key: KeyEvent) {
        match key.code {
            KeyCode::Down => {
                if let Some(Modal::CommandPalette(s)) = &mut self.state.modal
                    && !s.filtered.is_empty()
                {
                    s.selected = (s.selected + 1) % s.filtered.len();
                }
            }
            KeyCode::Up => {
                if let Some(Modal::CommandPalette(s)) = &mut self.state.modal {
                    if !s.filtered.is_empty() && s.selected > 0 {
                        s.selected -= 1;
                    } else if !s.filtered.is_empty() {
                        s.selected = s.filtered.len() - 1;
                    }
                }
            }
            KeyCode::Enter => {
                let should_close = self.execute_palette_command();
                if should_close {
                    self.state.modal = None;
                }
            }
            KeyCode::Backspace => {
                if let Some(Modal::CommandPalette(s)) = &mut self.state.modal {
                    s.query.pop();
                    self.refilter_palette();
                }
            }
            KeyCode::Esc => {
                self.state.modal = None;
            }
            KeyCode::Char(c) => {
                if let Some(Modal::CommandPalette(s)) = &mut self.state.modal {
                    s.query.push(c);
                    self.refilter_palette();
                }
            }
            _ => {}
        }
    }

    pub(super) fn open_command_palette(&mut self) {
        let commands = self.build_palette_commands();
        let filtered: Vec<usize> = (0..commands.len()).collect();
        self.state.modal = Some(Modal::CommandPalette(CommandPaletteState {
            query: String::new(),
            all_commands: commands,
            filtered,
            selected: 0,
        }));
    }

    pub(super) fn open_observability(&mut self) {
        self.state.modal = Some(Modal::Observability(ObservabilityState {
            scroll_offset: 0,
        }));
    }

    fn build_palette_commands(&self) -> Vec<PaletteCommand> {
        let mut cmds = Vec::new();

        for agent in &self.state.agents {
            match agent.status {
                AgentStatus::Idle | AgentStatus::Available | AgentStatus::Error(_) => {
                    if agent.config.is_some() {
                        cmds.push(PaletteCommand {
                            label: format!("Connect {}", agent.name),
                            action: CommandAction::ConnectAgent(agent.name.clone()),
                        });
                    }
                }
                AgentStatus::Connected | AgentStatus::Busy => {
                    cmds.push(PaletteCommand {
                        label: format!("Disconnect {}", agent.name),
                        action: CommandAction::DisconnectAgent(agent.name.clone()),
                    });
                }
                AgentStatus::Connecting | AgentStatus::Installing { .. } => {}
            }
        }

        // Session commands.
        for session in &self.state.session_store.active {
            cmds.push(PaletteCommand {
                label: format!("Switch to tab: #{} {}", session.id.0, session.agent_id),
                action: CommandAction::SwitchTab(session.agent_id.clone()),
            });
        }
        cmds.push(PaletteCommand {
            label: "New tab...".to_string(),
            action: CommandAction::NewTab,
        });
        if !self.state.session_store.active.is_empty() {
            cmds.push(PaletteCommand {
                label: "Close current tab".to_string(),
                action: CommandAction::CloseTab,
            });
        }

        cmds.push(PaletteCommand {
            label: "Toggle observability".to_string(),
            action: CommandAction::ToggleObservability,
        });
        cmds.push(PaletteCommand {
            label: "Clear conversation".to_string(),
            action: CommandAction::ClearConversation,
        });
        cmds.push(PaletteCommand {
            label: "Show help".to_string(),
            action: CommandAction::ShowHelp,
        });

        cmds
    }

    fn refilter_palette(&mut self) {
        if let Some(Modal::CommandPalette(s)) = &mut self.state.modal {
            let query = s.query.to_lowercase();
            s.filtered = s
                .all_commands
                .iter()
                .enumerate()
                .filter(|(_, cmd)| cmd.label.to_lowercase().contains(&query))
                .map(|(i, _)| i)
                .collect();
            s.selected = 0;
        }
    }

    fn execute_palette_command(&mut self) -> bool {
        let action = if let Some(Modal::CommandPalette(s)) = &self.state.modal {
            s.filtered
                .get(s.selected)
                .and_then(|&idx| s.all_commands.get(idx))
                .map(|cmd| cmd.action.clone())
        } else {
            return true;
        };

        match action {
            Some(CommandAction::ToggleObservability) => {
                self.state.modal = None;
                self.open_observability();
                false
            }
            Some(CommandAction::ShowHelp) => {
                self.state.modal = Some(Modal::Help);
                false
            }
            Some(CommandAction::ConnectAgent(name)) => {
                self.connect_agent(&name);
                let session_idx = self.ensure_session_for_agent(&name);
                self.switch_session(session_idx);
                true
            }
            Some(CommandAction::DisconnectAgent(name)) => {
                self.disconnect_agent(&name);
                true
            }
            Some(CommandAction::ClearConversation) => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    *sb = ScrollbackState::new();
                }
                true
            }
            Some(CommandAction::NewTab) => {
                self.state.modal = None;
                self.state.mode = super::InputMode::Agent;
                false
            }
            Some(CommandAction::CloseTab) => {
                self.close_current_session();
                true
            }
            Some(CommandAction::SwitchTab(name)) => {
                if let Some(idx) = self.session_for_agent(&name) {
                    self.switch_session(idx);
                }
                true
            }
            None => true,
        }
    }
}
