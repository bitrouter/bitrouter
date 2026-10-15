//! Bounded, read-only pages over the daemon's immutable operation history.
use super::{Effect, Screen, State};
use crossterm::event::KeyCode;
use std::collections::VecDeque;

#[derive(Default)]
pub(super) struct History {
    pub goal_id: String,
    pub epoch: u64,
    pub after: u64,
    pub cutoff: Option<u64>,
    pub next: Option<u64>,
    pub previous: VecDeque<u64>,
    pub lines: Vec<String>,
    pub offset: usize,
    pub pending: bool,
    pub error: Option<String>,
}

impl History {
    fn request(&mut self) -> Effect {
        self.epoch = self.epoch.saturating_add(1);
        self.pending = true;
        self.error = None;
        self.offset = 0;
        Effect::ReadHistory {
            goal_id: self.goal_id.clone(),
            after: self.after,
            cutoff: self.cutoff,
            epoch: self.epoch,
        }
    }
}

impl State {
    pub(super) fn open_history(&mut self, goal_id: String) -> Effect {
        let epoch = self.history.as_ref().map_or(0, |page| page.epoch);
        let mut history = History {
            goal_id,
            epoch,
            ..Default::default()
        };
        let effect = history.request();
        self.history = Some(history);
        self.screen = Screen::History;
        effect
    }

    /// Ignore late replies from a different Goal or page. Reading never submits work.
    pub fn receive_history(
        &mut self,
        goal_id: &str,
        epoch: u64,
        result: Result<(Vec<String>, u64, Option<u64>), String>,
    ) {
        let Some(page) = &mut self.history else {
            return;
        };
        if page.goal_id != goal_id || page.epoch != epoch {
            return;
        }
        page.pending = false;
        match result {
            Ok((lines, cutoff, next)) => {
                page.lines = lines;
                page.cutoff = Some(cutoff);
                page.next = next;
                page.error = None;
            }
            Err(error) => page.error = Some(error),
        }
    }

    pub(super) fn history_event(&mut self, key: KeyCode) -> Vec<Effect> {
        if key == KeyCode::Esc {
            self.screen = Screen::Goal;
            return Vec::new();
        }
        let Some(page) = &mut self.history else {
            return Vec::new();
        };
        if page.pending {
            return Vec::new();
        }
        match key {
            KeyCode::Down => page.offset = page.offset.saturating_add(1),
            KeyCode::Up => page.offset = page.offset.saturating_sub(1),
            KeyCode::PageDown if page.next.is_some() => {
                if page.previous.len() == 64 {
                    page.previous.pop_front();
                }
                page.previous.push_back(page.after);
                page.after = page.next.unwrap_or(page.after);
                return vec![page.request()];
            }
            KeyCode::PageUp if !page.previous.is_empty() => {
                page.after = page.previous.pop_back().unwrap_or(0);
                return vec![page.request()];
            }
            KeyCode::Home | KeyCode::Char('r') => {
                page.after = 0;
                page.previous.clear();
                if key == KeyCode::Char('r') {
                    page.cutoff = None;
                }
                return vec![page.request()];
            }
            _ => {}
        }
        Vec::new()
    }
}
