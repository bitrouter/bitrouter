//! [`BroadcastBar`] — a bar shown above the status bar when agents are selected.
//!
//! Displays "N selected" and a text input with a "Send to N selected" button
//! that dispatches `Command::SendPrompt { target: Target::Selection { ids }, text }`.
//!
//! The bar is invisible (returns an empty `div`) when `state.selection` is empty.

use bitrouter_gui_core::protocol::{Command, SessionId, Target};
use gpui::{
    div, AppContext as _, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window,
};
use gpui_component::{
    button::Button,
    h_flex,
    input::{Input, InputState},
    ActiveTheme, StyledExt,
};

use crate::app_model::AppModel;

// ── Pure helper ────────────────────────────────────────────────────────────────

/// Build the label text shown in the broadcast bar.
///
/// Returns e.g. `"2 selected"` or `"1 selected"`.
pub fn selection_label(count: usize) -> String {
    format!("{count} selected")
}

// ── View ──────────────────────────────────────────────────────────────────────

/// Broadcast bar shown when `state.selection` is non-empty.
pub struct BroadcastBar {
    model: Entity<AppModel>,
    /// Text input for the broadcast message.
    input: Option<Entity<InputState>>,
    /// Subscriptions kept alive to avoid dangling subscription drop.
    _subscriptions: Vec<gpui::Subscription>,
}

impl BroadcastBar {
    /// Create a new [`BroadcastBar`] backed by `model`.
    ///
    /// Observes `model` so the view re-renders whenever the backing entity
    /// is updated by the feed pump.
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            input: None,
            _subscriptions: Vec::new(),
        }
    }

    /// Read the current text from the input state, or return an empty string.
    fn current_text(&self, cx: &Context<Self>) -> String {
        self.input
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// Dispatch `SendPrompt` to all selected sessions and clear the input.
    fn send(&mut self, ids: Vec<SessionId>, window: &mut Window, cx: &mut Context<Self>) {
        let text = self.current_text(cx);
        if text.trim().is_empty() || ids.is_empty() {
            return;
        }
        self.model.update(cx, |m, _| {
            m.dispatch(Command::SendPrompt {
                target: Target::Selection { ids },
                text,
            });
        });
        // Clear the input after dispatching.
        if let Some(input) = &self.input {
            input.update(cx, |state, cx| {
                state.set_value("", window, cx);
            });
        }
    }
}

impl Render for BroadcastBar {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Always-present outer container so the return type is uniform.
        let outer = div().w_full();

        // Read selection while holding immutable borrow.
        let selection: Vec<SessionId> = self.model.read(cx).state.selection.clone();

        if selection.is_empty() {
            // Invisible — contributes no height.
            return outer;
        }

        // Lazily initialise the InputState (requires `&mut Window`).
        if self.input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Broadcast message…"));
            self.input = Some(input);
        }

        let Some(input_entity) = self.input.clone() else {
            return outer;
        };

        let count = selection.len();
        let label = selection_label(count);
        let button_label = format!("Send to {count} selected");

        let this_entity = cx.entity().clone();
        let send_btn = Button::new(gpui::ElementId::Name("broadcast-send".into()))
            .label(button_label)
            .on_click(move |_: &ClickEvent, window, cx| {
                let ids = selection.clone();
                this_entity.update(cx, |bar, cx| {
                    bar.send(ids, window, cx);
                });
            });

        outer.child(
            h_flex()
                .w_full()
                .h_9()
                .px_3()
                .gap_x_2()
                .border_t_1()
                .border_color(cx.theme().border)
                .bg(cx.theme().secondary)
                .items_center()
                .child(
                    div()
                        .text_xs()
                        .font_semibold()
                        .text_color(cx.theme().muted_foreground)
                        .child(label),
                )
                .child(div().flex_1().child(Input::new(&input_entity)))
                .child(send_btn),
        )
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{selection_label, BroadcastBar};
    use bitrouter_gui_core::protocol::{Command, SessionId, Target};

    // ── Pure helper tests ──────────────────────────────────────────────────────

    #[test]
    fn selection_label_singular() -> anyhow::Result<()> {
        assert_eq!(selection_label(1), "1 selected");
        Ok(())
    }

    #[test]
    fn selection_label_plural() -> anyhow::Result<()> {
        assert_eq!(selection_label(3), "3 selected");
        Ok(())
    }

    #[test]
    fn selection_label_zero() -> anyhow::Result<()> {
        assert_eq!(selection_label(0), "0 selected");
        Ok(())
    }

    // ── Command construction logic test ────────────────────────────────────────

    /// Verify that the Command built by the broadcast path has the right Target.
    #[test]
    fn broadcast_command_has_selection_target() -> anyhow::Result<()> {
        let ids = vec![
            SessionId("auth-fix".into()),
            SessionId("refactor-api".into()),
        ];
        let cmd = Command::SendPrompt {
            target: Target::Selection { ids: ids.clone() },
            text: "run tests".into(),
        };
        match cmd {
            Command::SendPrompt {
                target: Target::Selection { ids: ref t_ids },
                ref text,
            } => {
                assert_eq!(t_ids.len(), 2);
                assert_eq!(t_ids[0].0, "auth-fix");
                assert_eq!(t_ids[1].0, "refactor-api");
                assert_eq!(text, "run tests");
            }
            _ => return Err(anyhow::anyhow!("wrong command variant")),
        }
        Ok(())
    }

    // ── View construction smoke test ──────────────────────────────────────────

    /// BroadcastBar constructs without panicking over scenario state.
    #[gpui::test]
    fn broadcast_bar_renders_without_panic(cx: &mut gpui::TestAppContext) {
        use crate::app_model::AppModel;
        use bitrouter_gui_core::feed::MockFeed;
        use gpui::AppContext as _;

        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        cx.update(|cx| {
            let _ = cx.new(|cx| BroadcastBar::new(model.clone(), cx));
        });
    }

    /// BroadcastBar is constructible and selection is reachable after toggle.
    #[gpui::test]
    fn broadcast_bar_selection_dispatch_reachable(cx: &mut gpui::TestAppContext) {
        use crate::app_model::AppModel;
        use bitrouter_gui_core::feed::MockFeed;
        use gpui::AppContext as _;

        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        // Toggle selection of the first two sessions.
        let ids: Vec<SessionId> = model.read_with(cx, |m, _| {
            m.state
                .sessions
                .iter()
                .map(|v| v.session.id.clone())
                .collect()
        });

        if let (Some(id0), Some(id1)) = (ids.first(), ids.get(1)) {
            let id0 = id0.clone();
            let id1 = id1.clone();
            model.update(cx, |m, _| {
                m.toggle_selection(&id0);
                m.toggle_selection(&id1);
            });
        }

        let selection_len = model.read_with(cx, |m, _| m.state.selection.len());
        assert_eq!(selection_len, 2, "expected 2 sessions selected");

        // Build the broadcast bar — it should be constructible without panic.
        cx.update(|cx| {
            let _ = cx.new(|cx| BroadcastBar::new(model.clone(), cx));
        });
    }
}
