//! [`Center`] — the main content pane.
//!
//! Hosts either a live [`TerminalView`] (when `render_mode == Terminal`) or the
//! ACP transcript view (when `render_mode == Acp`), for the currently-focused
//! session.
//!
//! Terminal entities are cached per session id so switching focus does NOT
//! respawn terminals; a new PTY is created only the first time a session is
//! focused in terminal mode.

use std::collections::HashMap;

use bitrouter_gui_core::protocol::{RenderMode, SessionId};
use gpui::{
    div, prelude::FluentBuilder as _, AppContext as _, ClickEvent, Context, Entity, IntoElement,
    ParentElement, Render, Styled, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, StyledExt,
};

use crate::{
    app_model::AppModel,
    terminal::{entity::Terminal, view::TerminalView},
    views::{permission_modal::render_permission_modal, transcript::render_transcript},
};

/// Shell of the currently-focused agent session.
pub struct Center {
    model: Entity<AppModel>,
    /// One `TerminalView` entity per session, lazily constructed on first focus.
    terminal_cache: HashMap<SessionId, Entity<TerminalView>>,
}

impl Center {
    /// Create a [`Center`] view backed by `model`.
    ///
    /// Observes `model` so the view re-renders whenever the backing entity
    /// is updated by the feed pump.
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            terminal_cache: HashMap::new(),
        }
    }

    /// Return (or lazily create) the cached [`TerminalView`] for `id`.
    fn terminal_for(&mut self, id: &SessionId, cx: &mut Context<Self>) -> Entity<TerminalView> {
        if let Some(view) = self.terminal_cache.get(id) {
            return view.clone();
        }

        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
        let term = cx.new(|cx| match Terminal::spawn(&shell, &[], None, 24, 80, cx) {
            Ok(t) => t,
            Err(e) => {
                eprintln!("terminal spawn failed for {}: {e}", id.0);
                Terminal::placeholder()
            }
        });
        let view = cx.new(|cx| TerminalView::new(term, cx));
        self.terminal_cache.insert(id.clone(), view.clone());
        view
    }
}

impl Render for Center {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // ── Snapshot state before any mutable work ────────────────────────
        //
        // Read everything from the model while holding only an immutable borrow,
        // then release it before calling `terminal_for` (which needs `&mut cx`).
        let snapshot = {
            let state = &self.model.read(cx).state;
            let focus = state.focus.clone();
            focus.as_ref().and_then(|id| {
                state
                    .sessions
                    .iter()
                    .find(|v| &v.session.id == id)
                    .map(|sv| {
                        (
                            id.clone(),
                            sv.session.name.clone(),
                            sv.session.model.clone(),
                            sv.cost_micro_usd,
                            sv.session.render_mode,
                            sv.pending.clone(),
                            sv.transcript.clone(),
                        )
                    })
            })
        };
        // Immutable borrow released.

        // Outer wrapper — always a single `div` so the return type is uniform.
        let outer = div().flex_1().size_full().relative();

        let Some((id, name, model_name, cost_micro, render_mode, pending, transcript_items)) =
            snapshot
        else {
            // No focused session — show placeholder.
            return outer.child(
                v_flex()
                    .flex_1()
                    .size_full()
                    .items_center()
                    .justify_center()
                    .bg(cx.theme().background)
                    .child(
                        div()
                            .text_sm()
                            .text_color(cx.theme().muted_foreground)
                            .child("select an agent"),
                    ),
            );
        };

        // ── Header ────────────────────────────────────────────────────────
        let cost_label = {
            let dollars = cost_micro / 1_000_000;
            let cents = (cost_micro % 1_000_000) / 10_000;
            format!("${dollars}.{cents:02}")
        };
        let header_left = div()
            .text_sm()
            .font_semibold()
            .text_color(cx.theme().foreground)
            .child(format!("{name} · {model_name}  {cost_label}"));

        // Mode toggle button: shows current mode; clicking switches to the other.
        let toggle_label = match render_mode {
            RenderMode::Terminal => "terminal ▾",
            RenderMode::Acp => "transcript ▾",
        };
        let other_mode = match render_mode {
            RenderMode::Terminal => RenderMode::Acp,
            RenderMode::Acp => RenderMode::Terminal,
        };
        let toggle_id = id.clone();
        let model_clone = self.model.clone();
        let mode_toggle = Button::new(gpui::ElementId::Name("mode-toggle".into()))
            .ghost()
            .label(toggle_label)
            .on_click(move |_: &ClickEvent, _window, cx| {
                model_clone.update(cx, |m, cx| {
                    m.set_render_mode(&toggle_id, other_mode);
                    cx.notify();
                });
            });

        let header = h_flex()
            .w_full()
            .h_8()
            .px_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .items_center()
            .child(header_left)
            .child(div().flex_1())
            .child(mode_toggle);

        // ── Body: terminal or transcript ──────────────────────────────────
        let body: gpui::AnyElement = match render_mode {
            RenderMode::Terminal => {
                let term_view = self.terminal_for(&id, cx);
                div()
                    .flex_1()
                    .min_h_0()
                    .size_full()
                    .bg(cx.theme().background)
                    .child(term_view)
                    .into_any_element()
            }
            RenderMode::Acp => {
                // Build a synthetic SessionView from the snapshot so we can call
                // render_transcript without re-borrowing cx through the model.
                use bitrouter_gui_core::{
                    protocol::{RenderMode as RM, Session, SessionStatus, TabId},
                    state::SessionView,
                };
                let sv = SessionView {
                    session: Session {
                        id: id.clone(),
                        name: name.clone(),
                        tab: TabId(String::new()),
                        harness: String::new(),
                        model: model_name.clone(),
                        status: SessionStatus::Running,
                        render_mode: RM::Acp,
                    },
                    transcript: transcript_items,
                    pending: pending.clone(),
                    cost_micro_usd: cost_micro,
                    tokens_in: 0,
                    tokens_out: 0,
                    last_route: None,
                    failovers: 0,
                    latencies_ms: Vec::new(),
                };
                div()
                    .flex_1()
                    .min_h_0()
                    .size_full()
                    .bg(cx.theme().background)
                    .overflow_hidden()
                    .child(render_transcript(&sv, cx))
                    .into_any_element()
            }
        };

        // ── Permission modal overlay ───────────────────────────────────────
        let model_for_modal = self.model.clone();
        let maybe_modal = pending
            .as_ref()
            .map(|perm| render_permission_modal(perm, &id, model_for_modal, cx).into_any_element());

        // ── Compose full pane ─────────────────────────────────────────────
        let content = v_flex()
            .flex_1()
            .min_h_0()
            .size_full()
            .bg(cx.theme().background)
            .child(header)
            .child(body);

        outer
            .child(content)
            .when_some(maybe_modal, |el, modal| el.child(modal))
    }
}

// ── tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::Center;
    use crate::app_model::AppModel;
    use bitrouter_gui_core::feed::MockFeed;
    use gpui::{AppContext as _, TestAppContext};

    /// Build a Center view and run until parked — no panic.
    #[gpui::test]
    fn center_renders_without_panic(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        cx.update(|cx| {
            let _ = cx.new(|cx| Center::new(model.clone(), cx));
        });
    }

    /// scenario() auto-focuses the first session; verify Center is constructible
    /// with a focused model.
    #[gpui::test]
    fn center_constructible_with_focused_session(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        let focused = model.read_with(cx, |m, _| m.state.focus.clone());
        assert!(focused.is_some(), "expected a focused session");

        cx.update(|cx| {
            let _ = cx.new(|cx| Center::new(model.clone(), cx));
        });
    }
}
