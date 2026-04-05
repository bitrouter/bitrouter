use crate::model::{ScrollbackState, Tab, TabBadge};

use super::{App, InputMode};

impl App {
    /// Find the tab index for a given agent name.
    pub(super) fn tab_for_agent(&self, agent_name: &str) -> Option<usize> {
        self.state
            .tabs
            .iter()
            .position(|t| t.agent_name == agent_name)
    }

    /// Get a mutable reference to an agent's tab scrollback.
    pub(super) fn scrollback_for_agent(
        &mut self,
        agent_name: &str,
    ) -> Option<&mut ScrollbackState> {
        self.state
            .tabs
            .iter_mut()
            .find(|t| t.agent_name == agent_name)
            .map(|t| &mut t.scrollback)
    }

    /// Switch to a tab by index, clearing its badge and resetting search.
    pub(super) fn switch_tab(&mut self, idx: usize) {
        if idx < self.state.tabs.len() {
            self.state.active_tab = idx;
            self.state.tabs[idx].badge = TabBadge::None;
            // Search state references entries from the old tab — invalidate it.
            if self.state.search.is_some() {
                self.state.search = None;
                if self.state.mode == InputMode::Search {
                    self.state.mode = InputMode::Normal;
                }
            }
        }
    }

    /// Create a tab for an agent if one doesn't already exist. Returns the tab index.
    pub(super) fn ensure_tab(&mut self, agent_name: &str) -> usize {
        if let Some(idx) = self.tab_for_agent(agent_name) {
            return idx;
        }
        self.state.tabs.push(Tab {
            agent_name: agent_name.to_string(),
            scrollback: ScrollbackState::new(),
            badge: TabBadge::None,
        });
        self.state.tabs.len() - 1
    }

    /// Increment unread badge on a background tab.
    pub(super) fn badge_background_tab(&mut self, agent_name: &str) {
        if let Some(idx) = self.tab_for_agent(agent_name)
            && idx != self.state.active_tab
        {
            let tab = &mut self.state.tabs[idx];
            tab.badge = match &tab.badge {
                TabBadge::None => TabBadge::Unread(1),
                TabBadge::Unread(n) => TabBadge::Unread(n + 1),
                TabBadge::Permission => TabBadge::Permission, // Don't downgrade
            };
        }
    }

    /// Close the current tab and disconnect its agent.
    pub(super) fn close_current_tab(&mut self) {
        if self.state.tabs.is_empty() {
            return;
        }
        let idx = self.state.active_tab;
        let agent_name = self.state.tabs[idx].agent_name.clone();

        // Disconnect the agent if connected.
        self.disconnect_agent(&agent_name);

        self.state.tabs.remove(idx);
        // Immediately clamp active_tab to valid range.
        self.state.active_tab = if self.state.tabs.is_empty() {
            0
        } else {
            idx.min(self.state.tabs.len() - 1)
        };
    }
}
