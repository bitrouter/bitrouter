//! [`AppModel`] — the single source of truth for rendered state.
//!
//! Connects a [`Feed`] to the pure [`state::reduce`] function and re-notifies
//! gpui on every event so all views redraw automatically.

use bitrouter_gui_core::feed::Feed;
use bitrouter_gui_core::protocol::{Command, RenderMode, SessionId};
use bitrouter_gui_core::state::{self, State};
use futures::channel::mpsc::UnboundedSender;
use futures::StreamExt;
use gpui::Context;

/// Owns the application [`State`] and the command channel to the daemon feed.
///
/// Construct with `cx.new(|cx| AppModel::new(feed, cx))` so the spawned event
/// loop task can hold a weak reference back to this entity.
pub struct AppModel {
    /// Current application state — views read directly from here.
    pub state: State,
    /// Sink for outbound [`Command`]s. Errors on a closed channel are silently
    /// ignored (the feed disconnected).
    commands: UnboundedSender<Command>,
}

impl AppModel {
    /// Connect `feed`, spawn the event-pump task, and return the initial model.
    pub fn new<F: Feed>(feed: F, cx: &mut Context<Self>) -> Self {
        let handle = feed.connect();
        let commands = handle.commands;
        let mut events = handle.events;

        cx.spawn(async move |this, cx| {
            while let Some(ev) = events.next().await {
                let update_result = this.update(cx, |model, cx| {
                    state::reduce(&mut model.state, ev);
                    cx.notify();
                });
                // If the entity has been dropped the update returns Err; stop the pump.
                if update_result.is_err() {
                    break;
                }
            }
        })
        .detach();

        Self {
            state: State::default(),
            commands,
        }
    }

    /// Send a [`Command`] to the daemon feed. Send errors are silently dropped
    /// (the connection may have closed).
    pub fn dispatch(&self, cmd: Command) {
        let _ = self.commands.unbounded_send(cmd);
    }

    /// Override the render mode for a single session without routing through the
    /// feed — this is local view state only.
    pub fn set_render_mode(&mut self, id: &SessionId, mode: RenderMode) {
        if let Some(view) = self.state.sessions.iter_mut().find(|v| &v.session.id == id) {
            view.session.render_mode = mode;
        }
    }

    /// Set the focused session. This is local view state only and does not
    /// go through the feed.
    pub fn set_focus(&mut self, id: &SessionId) {
        if self.state.sessions.iter().any(|v| &v.session.id == id) {
            self.state.focus = Some(id.clone());
        }
    }

    /// Toggle selection of a session (add if absent, remove if present).
    /// This is local view state only and does not go through the feed.
    pub fn toggle_selection(&mut self, id: &SessionId) {
        if let Some(pos) = self.state.selection.iter().position(|s| s == id) {
            self.state.selection.remove(pos);
        } else if self.state.sessions.iter().any(|v| &v.session.id == id) {
            self.state.selection.push(id.clone());
        }
    }

    /// Clear the pending permission for a session and restore its status to
    /// `Running`. Called locally after dispatching `ResolvePending` so the
    /// modal closes immediately without waiting for a feed event.
    pub fn resolve_pending(&mut self, id: &SessionId) {
        use bitrouter_gui_core::protocol::SessionStatus;
        if let Some(v) = self.state.sessions.iter_mut().find(|v| &v.session.id == id) {
            v.pending = None;
            if v.session.status == SessionStatus::WaitingPermission {
                v.session.status = SessionStatus::Running;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::AppModel;
    use bitrouter_gui_core::feed::MockFeed;
    use bitrouter_gui_core::protocol::{RenderMode, SessionId};
    use gpui::{AppContext as _, TestAppContext};

    /// `MockFeed::scenario` emits 3 `AgentSpawned` events plus extra events;
    /// after the executor drains them the state must hold exactly 3 sessions.
    #[gpui::test]
    fn scenario_populates_three_sessions(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));

        // Let the spawned event-pump task run to completion.
        cx.run_until_parked();

        let session_count = model.read_with(cx, |m, _| m.state.sessions.len());
        assert_eq!(session_count, 3);
    }

    /// `set_focus` must update `state.focus` without going through the feed.
    #[gpui::test]
    fn set_focus_updates_focus(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        let id = SessionId("refactor-api".into());
        model.update(cx, |m, _| m.set_focus(&id));

        let focus = model.read_with(cx, |m, _| m.state.focus.clone());
        assert!(matches!(focus, Some(ref f) if f.0 == "refactor-api"));
    }

    /// `toggle_selection` must add an id on first call and remove it on the second.
    #[gpui::test]
    fn toggle_selection_adds_then_removes(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        let id = SessionId("auth-fix".into());
        model.update(cx, |m, _| m.toggle_selection(&id));

        let len_after_add = model.read_with(cx, |m, _| m.state.selection.len());
        assert_eq!(len_after_add, 1);

        model.update(cx, |m, _| m.toggle_selection(&id));

        let len_after_remove = model.read_with(cx, |m, _| m.state.selection.len());
        assert_eq!(len_after_remove, 0);
    }

    /// `resolve_pending` clears the pending permission and sets status to Running.
    #[gpui::test]
    fn resolve_pending_clears_permission(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        // After the scenario, "refactor-api" has a pending permission.
        let id = SessionId("refactor-api".into());
        let has_pending_before = model.read_with(cx, |m, _| {
            m.state.session("refactor-api").map(|v| v.pending.is_some())
        });
        assert!(matches!(has_pending_before, Some(true)));

        model.update(cx, |m, _| m.resolve_pending(&id));

        let pending_after = model.read_with(cx, |m, _| {
            m.state.session("refactor-api").map(|v| v.pending.clone())
        });
        assert!(matches!(pending_after, Some(None)));
    }

    /// `set_render_mode` must mutate the matching session without going through
    /// the feed.
    #[gpui::test]
    fn set_render_mode_mutates_local_state(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        let id = SessionId("auth-fix".into());
        model.update(cx, |m, _| m.set_render_mode(&id, RenderMode::Acp));

        let mode = model.read_with(cx, |m, _| {
            m.state
                .sessions
                .iter()
                .find(|v| v.session.id.0 == "auth-fix")
                .map(|v| v.session.render_mode)
        });

        assert!(matches!(mode, Some(RenderMode::Acp)));
    }
}
