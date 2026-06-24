//! Global gpui actions and key bindings for BitRouter.
//!
//! # Keybindings
//!
//! | Shortcut | Action           | Effect                                      |
//! |----------|------------------|---------------------------------------------|
//! | ⌘K       | `OpenPalette`    | Open the command palette                    |
//! | ⌘N       | `OpenPalette`    | Same — "new agent" entry is first in list   |
//! | ⌘1–⌘9    | `FocusSession`   | Focus the Nth session (1-indexed)           |
//!
//! Call [`register`] once from `main` (inside `gpui_platform::application().run`)
//! to bind everything.

use gpui::{actions, App, KeyBinding};
use serde::Deserialize;

// ── Actions ───────────────────────────────────────────────────────────────────

// Open the command palette (triggered by ⌘K or ⌘N).
actions!(bitrouter, [OpenPalette]);

/// Focus the Nth session (1-indexed). `n` is in `[1, 9]`.
#[derive(Clone, PartialEq, Eq, Deserialize, gpui::Action)]
#[action(namespace = bitrouter, no_json)]
pub struct FocusSession {
    pub n: usize,
}

// ── Key registration ──────────────────────────────────────────────────────────

/// Register all BitRouter key bindings on `cx`.
///
/// Call this once at application startup, before opening any window.
pub fn register(cx: &mut App) {
    cx.bind_keys([
        KeyBinding::new("cmd-k", OpenPalette, None),
        KeyBinding::new("cmd-n", OpenPalette, None),
        KeyBinding::new("cmd-1", FocusSession { n: 1 }, None),
        KeyBinding::new("cmd-2", FocusSession { n: 2 }, None),
        KeyBinding::new("cmd-3", FocusSession { n: 3 }, None),
        KeyBinding::new("cmd-4", FocusSession { n: 4 }, None),
        KeyBinding::new("cmd-5", FocusSession { n: 5 }, None),
        KeyBinding::new("cmd-6", FocusSession { n: 6 }, None),
        KeyBinding::new("cmd-7", FocusSession { n: 7 }, None),
        KeyBinding::new("cmd-8", FocusSession { n: 8 }, None),
        KeyBinding::new("cmd-9", FocusSession { n: 9 }, None),
    ]);
}

// ── Pure helper ───────────────────────────────────────────────────────────────

/// Return the `SessionId` that corresponds to the 1-indexed position `n` in
/// `sessions`. Returns `None` when `n` is 0 or out of range.
pub fn nth_session_id(
    sessions: &[bitrouter_gui_core::protocol::SessionId],
    n: usize,
) -> Option<bitrouter_gui_core::protocol::SessionId> {
    if n == 0 {
        return None;
    }
    sessions.get(n - 1).cloned()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::nth_session_id;
    use bitrouter_gui_core::protocol::SessionId;

    fn ids(n: usize) -> Vec<SessionId> {
        (1..=n).map(|i| SessionId(format!("s{i}"))).collect()
    }

    #[test]
    fn nth_session_id_valid_indices() -> anyhow::Result<()> {
        let sessions = ids(3);
        let first = nth_session_id(&sessions, 1).ok_or_else(|| anyhow::anyhow!("missing 1"))?;
        assert_eq!(first.0, "s1");
        let third = nth_session_id(&sessions, 3).ok_or_else(|| anyhow::anyhow!("missing 3"))?;
        assert_eq!(third.0, "s3");
        Ok(())
    }

    #[test]
    fn nth_session_id_out_of_range_returns_none() -> anyhow::Result<()> {
        let sessions = ids(2);
        assert!(nth_session_id(&sessions, 0).is_none());
        assert!(nth_session_id(&sessions, 3).is_none());
        Ok(())
    }

    #[test]
    fn nth_session_id_empty_always_none() -> anyhow::Result<()> {
        assert!(nth_session_id(&[], 1).is_none());
        Ok(())
    }

    /// Focus mapping: build AppModel from scenario, call nth_session_id,
    /// then set_focus — verify state.focus is updated correctly.
    #[gpui::test]
    fn focus_mapping_via_set_focus(cx: &mut gpui::TestAppContext) {
        use crate::app_model::AppModel;
        use bitrouter_gui_core::feed::MockFeed;
        use gpui::AppContext as _;

        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        // Get session IDs in order.
        let session_ids: Vec<bitrouter_gui_core::protocol::SessionId> =
            model.read_with(cx, |m, _| {
                m.state
                    .sessions
                    .iter()
                    .map(|v| v.session.id.clone())
                    .collect()
            });

        // Focus the 2nd session (index 1 in the list).
        if let Some(target_id) = nth_session_id(&session_ids, 2) {
            model.update(cx, |m, _| m.set_focus(&target_id));
            let focus = model.read_with(cx, |m, _| m.state.focus.clone());
            assert!(focus.is_some_and(|f| f == target_id));
        }
    }
}
