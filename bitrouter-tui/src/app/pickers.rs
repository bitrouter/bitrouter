//! Inline picker micro-mode.
//!
//! When a slash command needs the user to choose from a list (an
//! agent for `/session new`, a session for `/session switch`,
//! on-disk sessions for `/session import`), the TUI renders the list
//! as a [`PickerEntry`] inside the active session's scrollback and
//! enters [`InputMode::Picker`]. While the picker is open `j/k`,
//! Enter, Esc, and Space are scoped to it; other input is suspended.

use crossterm::event::{KeyCode, KeyEvent};

use crate::model::{ActivityEntry, EntryKind, PickerAction, PickerEntry, PickerOutcome};

use super::{App, InputMode};

/// Move the picker cursor by `delta` (1 forward, -1 backward),
/// skipping non-selectable rows. Wraps at the ends. Returns whether
/// the cursor actually moved (always true unless there are no
/// selectable items at all).
fn step_picker_cursor(picker: &mut PickerEntry, delta: i32) -> bool {
    let len = picker.items.len();
    if len == 0 {
        return false;
    }
    let mut next = picker.cursor as i32;
    for _ in 0..len as i32 {
        next = (next + delta).rem_euclid(len as i32);
        if let Some(it) = picker.items.get(next as usize)
            && it.selectable
        {
            picker.cursor = next as usize;
            return true;
        }
    }
    false
}

impl App {
    /// Open a picker entry on the active session's scrollback.
    /// Switches to [`InputMode::Picker`].
    pub(super) fn open_picker(
        &mut self,
        title: String,
        items: Vec<crate::model::PickerItem>,
        action: PickerAction,
        multiselect: bool,
    ) {
        let Some(sb) = self.state.active_scrollback_mut() else {
            return;
        };
        // Anchor the cursor on the first selectable row so j/k from
        // the start always lands the user on a usable item.
        let cursor = items.iter().position(|it| it.selectable).unwrap_or(0);
        if items.is_empty() || !items.iter().any(|it| it.selectable) {
            let id = sb.next_id();
            sb.push_entry(ActivityEntry {
                id,
                kind: EntryKind::System(crate::model::SystemNotice {
                    text: format!("{title}: (nothing to pick)"),
                }),
                collapsed: false,
            });
            return;
        }
        let id = sb.next_id();
        sb.push_entry(ActivityEntry {
            id,
            kind: EntryKind::Picker(PickerEntry {
                title,
                items,
                cursor,
                selected: std::collections::HashSet::new(),
                multiselect,
                action,
                outcome: PickerOutcome::Open,
            }),
            collapsed: false,
        });
        sb.follow = true;
        self.state.mode = InputMode::Picker;
    }

    pub(super) fn handle_picker_key(&mut self, key: KeyEvent) {
        // Locate the open picker on the active scrollback.
        let active_idx = self.state.active_session;
        let Some(session) = self.state.session_store.active.get_mut(active_idx) else {
            self.state.mode = InputMode::Normal;
            return;
        };
        let sb = &mut session.scrollback;
        let entry_idx = sb.entries.iter().rposition(
            |e| matches!(&e.kind, EntryKind::Picker(p) if matches!(p.outcome, PickerOutcome::Open)),
        );
        let Some(entry_idx) = entry_idx else {
            self.state.mode = InputMode::Normal;
            return;
        };
        let EntryKind::Picker(picker) = &mut sb.entries[entry_idx].kind else {
            self.state.mode = InputMode::Normal;
            return;
        };

        match key.code {
            KeyCode::Esc => {
                picker.outcome = PickerOutcome::Cancelled;
                sb.invalidate_entry(entry_idx);
                self.state.mode = InputMode::Normal;
            }
            KeyCode::Enter => {
                let chosen: Vec<usize> = if picker.multiselect {
                    let mut v: Vec<usize> = picker.selected.iter().copied().collect();
                    v.sort_unstable();
                    v
                } else {
                    vec![picker.cursor]
                };
                let action = picker.action.clone();
                picker.outcome = PickerOutcome::Confirmed {
                    indices: chosen.clone(),
                };
                sb.invalidate_entry(entry_idx);
                self.state.mode = InputMode::Normal;
                self.dispatch_picker_action(action, chosen);
            }
            KeyCode::Down | KeyCode::Char('j') => {
                if step_picker_cursor(picker, 1) {
                    sb.invalidate_entry(entry_idx);
                }
            }
            KeyCode::Up | KeyCode::Char('k') => {
                if step_picker_cursor(picker, -1) {
                    sb.invalidate_entry(entry_idx);
                }
            }
            KeyCode::Char(' ') if picker.multiselect => {
                let cur = picker.cursor;
                if !picker.selected.insert(cur) {
                    picker.selected.remove(&cur);
                }
                sb.invalidate_entry(entry_idx);
            }
            _ => {}
        }
    }

    fn dispatch_picker_action(&mut self, action: PickerAction, indices: Vec<usize>) {
        match action {
            PickerAction::NewSession { agents } => {
                if let Some(&i) = indices.first()
                    && let Some(name) = agents.get(i)
                {
                    let len_before = self.state.session_store.active.len();
                    self.connect_agent(name);
                    let len_after = self.state.session_store.active.len();
                    if len_after > len_before {
                        self.switch_session(len_after - 1);
                    }
                }
            }
            PickerAction::SwitchSession { ids } => {
                if let Some(&i) = indices.first()
                    && let Some(target_id) = ids.get(i)
                    && let Some(idx) = self.state.session_store.index_of(*target_id)
                {
                    self.switch_session(idx);
                }
            }
            PickerAction::Import { candidates } => {
                let mut imported = 0usize;
                for &i in &indices {
                    if let Some(c) = candidates.get(i) {
                        self.import_session(
                            &c.agent_id,
                            c.external_session_id.clone(),
                            c.source_path.clone(),
                            c.title_hint.clone(),
                        );
                        imported += 1;
                    }
                }
                if imported > 0 {
                    self.push_system_msg(&format!("Importing {imported} session(s)..."));
                }
                let _ = self.write_import_marker();
            }
        }
    }
}
