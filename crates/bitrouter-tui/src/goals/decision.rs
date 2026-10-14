//! Pinned actual permission requests. Viewing is never consent.
use super::{Decision, Effect, Screen, State};
use crossterm::event::KeyCode;

pub(super) struct Dialog {
    pub goal_id: String,
    pub choices: Vec<Decision>,
    pub index: usize,
    pub selected: Option<usize>,
    pub material: Option<Result<String, String>>,
    pub offset: usize,
    pub stale: bool,
}

impl Dialog {
    pub(super) fn new(goal_id: String, choices: Vec<Decision>) -> Self {
        Self {
            goal_id,
            choices,
            index: 0,
            selected: None,
            material: None,
            offset: 0,
            stale: false,
        }
    }
    pub(super) fn current(&self) -> Option<&Decision> {
        self.choices.get(self.index)
    }
    pub(super) fn inspect(&self) -> Option<Effect> {
        self.current().map(|decision| Effect::Inspect {
            goal_id: self.goal_id.clone(),
            artifact_id: decision.request.id.clone(),
        })
    }
    pub(super) fn request_capacity(&self, height: u16, notice: bool) -> usize {
        let remaining =
            usize::from(height.saturating_sub(2)).saturating_sub(5 + usize::from(notice));
        let count = self.current().map_or(0, |decision| decision.options.len());
        let option_rows = count.min(remaining.saturating_sub(2).clamp(1, 3));
        remaining.saturating_sub(option_rows + usize::from(count > option_rows))
    }
}

impl State {
    pub fn permission_evidence_is_open(&self, goal_id: &str, artifact_id: &str) -> bool {
        self.screen == Screen::Goal
            && self.current.as_deref() == Some(goal_id)
            && self.decision.as_ref().is_some_and(|dialog| {
                !dialog.stale
                    && dialog.goal_id == goal_id
                    && dialog
                        .current()
                        .is_some_and(|decision| decision.request.id == artifact_id)
            })
    }
    /// Only the currently opened exact artifact can consume an asynchronous read.
    pub fn receive_evidence(
        &mut self,
        goal_id: &str,
        artifact_id: &str,
        result: Result<String, String>,
    ) {
        if self.current.as_deref() != Some(goal_id) {
            return;
        }
        if self.screen == Screen::Goal
            && let Some(dialog) = &mut self.decision
            && !dialog.stale
            && dialog.goal_id == goal_id
            && dialog
                .current()
                .is_some_and(|decision| decision.request.id == artifact_id)
        {
            dialog.material = Some(result);
        } else if self.screen == Screen::Detail
            && self
                .detail
                .as_ref()
                .and_then(|task| {
                    task.artifacts
                        .get(self.notebook().map_or(0, |book| book.selected_artifact))
                })
                .is_some_and(|artifact| artifact.id == artifact_id)
        {
            match result {
                Ok(text) => self.artifact_preview = Some(text),
                Err(reason) => self.notice = Some(format!("Evidence unavailable: {reason}")),
            }
        }
    }

    pub(super) fn decision_event(&mut self, code: KeyCode) -> Vec<Effect> {
        if code == KeyCode::Esc {
            self.decision = None;
            return Vec::new();
        }
        let height = self.viewport.map_or(24, |(_, height)| height);
        let notice = self.notice.is_some();
        let Some(dialog) = &mut self.decision else {
            return Vec::new();
        };
        if dialog.stale {
            return Vec::new();
        }
        let page = dialog.request_capacity(height, notice).max(1);
        match code {
            KeyCode::Tab | KeyCode::BackTab if dialog.choices.len() > 1 => {
                dialog.index = if code == KeyCode::Tab {
                    (dialog.index + 1) % dialog.choices.len()
                } else {
                    (dialog.index + dialog.choices.len() - 1) % dialog.choices.len()
                };
                dialog.selected = None;
                dialog.material = None;
                dialog.offset = 0;
                return dialog.inspect().into_iter().collect();
            }
            KeyCode::Char('r') => {
                dialog.material = None;
                return dialog.inspect().into_iter().collect();
            }
            KeyCode::PageUp => dialog.offset = dialog.offset.saturating_sub(page),
            KeyCode::PageDown => dialog.offset = dialog.offset.saturating_add(page),
            KeyCode::Up => dialog.selected = Some(dialog.selected.unwrap_or(1).saturating_sub(1)),
            KeyCode::Down => {
                let count = dialog
                    .current()
                    .map_or(0, |decision| decision.options.len());
                dialog.selected = Some(
                    dialog
                        .selected
                        .map_or(0, |index| index.saturating_add(1))
                        .min(count.saturating_sub(1)),
                );
            }
            KeyCode::Enter => {
                if let Some(decision) = dialog.current()
                    && let Some(option) = dialog
                        .selected
                        .and_then(|index| decision.options.get(index))
                {
                    if option.allows && !dialog.material.as_ref().is_some_and(Result::is_ok) {
                        self.notice = Some("Allow is unavailable until this exact request evidence has loaded. Retry or explicitly deny; no default consent.".into());
                        return Vec::new();
                    }
                    let effect = Effect::Respond {
                        goal_id: dialog.goal_id.clone(),
                        decision_id: decision.id.clone(),
                        requirements_revision: decision.requirements_revision,
                        option_id: option.id.clone(),
                    };
                    self.decision = None;
                    return vec![effect];
                }
            }
            _ => {}
        }
        Vec::new()
    }
}
