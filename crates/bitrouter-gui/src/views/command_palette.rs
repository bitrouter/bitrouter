//! [`CommandPalette`] — ⌘K natural-language + fuzzy command overlay.
//!
//! When open the palette renders a centered modal overlay with:
//! - A text [`Input`] for the query.
//! - A filtered list of static fuzzy actions below it.
//!
//! **Submit paths:**
//! 1. If the typed text matches a fuzzy action → dispatch that action immediately
//!    and close.
//! 2. Otherwise → send the text to the genai client on a background `std::thread`,
//!    dispatch the returned [`Command`]s on the model, then close. On error, keep
//!    the palette open and display an inline error message.
//!
//! The real [`⌘K`] key binding is wired in task 2.12; for now a button in the
//! title bar triggers `open()`.

use std::sync::mpsc;

use bitrouter_gui_core::protocol::{Command, RenderMode, SessionId, TabId, Target};
use gpui::{
    div, AppContext as _, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Render, StatefulInteractiveElement as _, Styled, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex,
    input::{Input, InputState},
    v_flex, ActiveTheme, StyledExt,
};

use crate::{ai::GenAiIntentClient, app_model::AppModel};

// ── Static action list ────────────────────────────────────────────────────────

/// The full list of static fuzzy actions shown in the palette.
const ACTIONS: &[&str] = &[
    "New agent",
    "Stop agent",
    "Toggle render mode",
    "Focus next",
];

// ── Pure helper ───────────────────────────────────────────────────────────────

/// Return all `actions` whose label contains `query` as a case-insensitive
/// substring. Returns all actions when `query` is empty.
pub fn filter_actions(actions: &[&str], query: &str) -> Vec<String> {
    let lower = query.to_lowercase();
    actions
        .iter()
        .filter(|a| lower.is_empty() || a.to_lowercase().contains(&lower))
        .map(|a| (*a).to_string())
        .collect()
}

// ── View ──────────────────────────────────────────────────────────────────────

/// The command-palette overlay.
///
/// Hold this as an `Entity<CommandPalette>` inside [`Root`] and call
/// [`CommandPalette::open`] to show it.
pub struct CommandPalette {
    model: Entity<AppModel>,
    /// Whether the palette is currently visible.
    open: bool,
    /// Text input entity — created lazily on the first render call that has a
    /// `Window` reference. (InputState::new requires `&mut Window`.)
    input: Option<Entity<InputState>>,
    /// Inline error message shown when the genai call fails.
    error: Option<String>,
    /// Receiver end of the genai background-thread channel.
    pending_rx: Option<mpsc::Receiver<anyhow::Result<Vec<Command>>>>,
    /// Subscriptions kept alive for the input change event.
    _subscriptions: Vec<gpui::Subscription>,
}

impl CommandPalette {
    /// Create a new (closed) palette backed by `model`.
    ///
    /// Observes `model` so the view re-renders whenever the backing entity
    /// is updated by the feed pump.
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self {
            model,
            open: false,
            input: None,
            error: None,
            pending_rx: None,
            _subscriptions: Vec::new(),
        }
    }

    /// Open the palette. Call this from the title-bar button or (later) the ⌘K
    /// key binding in task 2.12.
    pub fn open(&mut self, cx: &mut Context<Self>) {
        self.open = true;
        self.error = None;
        cx.notify();
    }

    /// Close the palette and clear transient state.
    fn close(&mut self, cx: &mut Context<Self>) {
        self.open = false;
        self.error = None;
        self.pending_rx = None;
        cx.notify();
    }

    /// Return the current text in the input, or an empty string.
    fn current_text(&self, cx: &Context<Self>) -> String {
        self.input
            .as_ref()
            .map(|i| i.read(cx).value().to_string())
            .unwrap_or_default()
    }

    /// Dispatch a well-known fuzzy action by name.
    fn dispatch_action(&mut self, action: &str, cx: &mut Context<Self>) {
        let state = &self.model.read(cx).state;
        let focus = state.focus.clone();
        match action {
            "New agent" => {
                // Spawn a placeholder agent on the default tab.
                self.model.update(cx, |m, _| {
                    m.dispatch(Command::SpawnAgent {
                        agent_id: "new-agent".into(),
                        model: "claude/claude-sonnet-4-5".into(),
                        worktree: None,
                        tab: TabId("main".into()),
                        prompt: None,
                    });
                });
            }
            "Stop agent" => {
                if let Some(id) = focus {
                    self.model.update(cx, |m, _| {
                        m.dispatch(Command::StopAgent {
                            target: Target::Session { id },
                        });
                    });
                }
            }
            "Toggle render mode" => {
                if let Some(id) = focus {
                    self.model.update(cx, |m, cx| {
                        let current = m
                            .state
                            .sessions
                            .iter()
                            .find(|v| v.session.id == id)
                            .map(|v| v.session.render_mode);
                        if let Some(mode) = current {
                            let next = match mode {
                                RenderMode::Terminal => RenderMode::Acp,
                                RenderMode::Acp => RenderMode::Terminal,
                            };
                            m.set_render_mode(&id, next);
                            cx.notify();
                        }
                    });
                }
            }
            "Focus next" => {
                // Advance focus to the next session in the list.
                let sessions: Vec<SessionId> = state
                    .sessions
                    .iter()
                    .map(|v| v.session.id.clone())
                    .collect();
                if !sessions.is_empty() {
                    let next_idx = focus
                        .and_then(|f| sessions.iter().position(|id| id == &f))
                        .map(|i| (i + 1) % sessions.len())
                        .unwrap_or(0);
                    let next_id = sessions[next_idx].clone();
                    self.model.update(cx, |m, cx| {
                        m.set_focus(&next_id);
                        cx.notify();
                    });
                }
            }
            _ => {}
        }
    }

    /// Submit the current input: either run a fuzzy action or kick off the
    /// genai background thread.
    fn submit(&mut self, cx: &mut Context<Self>) {
        let text = self.current_text(cx);
        if text.trim().is_empty() {
            self.close(cx);
            return;
        }

        // Check if the text exactly matches a single filtered action.
        let matched = filter_actions(ACTIONS, &text);
        if matched.len() == 1 {
            let action = matched[0].clone();
            self.dispatch_action(&action, cx);
            self.close(cx);
            return;
        }

        // Natural-language path: run genai on a background thread.
        let (tx, rx) = mpsc::channel::<anyhow::Result<Vec<Command>>>();
        let text_clone = text.clone();

        std::thread::spawn(move || {
            let result = GenAiIntentClient::new("claude/claude-sonnet-4-5")
                .and_then(|client| crate::ai::parse_intent(&client, &text_clone));
            // Ignore send error — the palette may have been closed already.
            let _ = tx.send(result);
        });

        self.pending_rx = Some(rx);
        self.error = None;
        cx.notify();
    }

    /// Poll the pending genai channel and, if a result is ready, dispatch
    /// the commands and close (or show an error). Call this from render.
    fn poll_pending(&mut self, cx: &mut Context<Self>) {
        let result = if let Some(rx) = &self.pending_rx {
            match rx.try_recv() {
                Ok(r) => Some(r),
                Err(mpsc::TryRecvError::Empty) => None,
                Err(mpsc::TryRecvError::Disconnected) => {
                    Some(Err(anyhow::anyhow!("genai thread disconnected")))
                }
            }
        } else {
            return;
        };

        if let Some(result) = result {
            self.pending_rx = None;
            match result {
                Ok(cmds) => {
                    for cmd in cmds {
                        self.model.update(cx, |m, _| m.dispatch(cmd));
                    }
                    self.close(cx);
                }
                Err(e) => {
                    self.error = Some(format!("Error: {e}"));
                    cx.notify();
                }
            }
        }
    }
}

impl Render for CommandPalette {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Lazily initialise the InputState the first time we have a Window.
        if self.input.is_none() {
            let input = cx.new(|cx| InputState::new(window, cx).placeholder("Type a command…"));
            self.input = Some(input);
        }

        // Poll for genai results every render tick while a request is pending.
        self.poll_pending(cx);

        // Outer scrim container — only visible when open.
        // Always rendered as the same type so the return is `impl IntoElement`.
        let outer = div().absolute().inset_0();

        if !self.open {
            // Return an invisible zero-contribution placeholder.
            return outer;
        }

        let Some(input_entity) = self.input.clone() else {
            return outer;
        };

        let text = self.current_text(cx);
        let filtered = filter_actions(ACTIONS, &text);
        let is_loading = self.pending_rx.is_some();

        // ── Error label ───────────────────────────────────────────────────
        let error_el = self.error.clone().map(|msg| {
            div()
                .text_xs()
                .text_color(cx.theme().danger)
                .child(msg)
                .into_any_element()
        });

        // ── Action list ───────────────────────────────────────────────────
        let this_entity = cx.entity().clone();
        let action_items: Vec<gpui::AnyElement> = filtered
            .into_iter()
            .map(|action_name| {
                let name_clone = action_name.clone();
                let entity_clone = this_entity.clone();
                div()
                    .id(gpui::ElementId::Name(
                        format!("palette-action-{}", action_name).into(),
                    ))
                    .w_full()
                    .px_3()
                    .py_1()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .rounded(cx.theme().radius)
                    .cursor_pointer()
                    .hover(|this| this.bg(cx.theme().secondary))
                    .on_click(move |_: &ClickEvent, _window, cx| {
                        entity_clone.update(cx, |palette, cx| {
                            palette.dispatch_action(&name_clone, cx);
                            palette.close(cx);
                        });
                    })
                    .child(action_name)
                    .into_any_element()
            })
            .collect();

        // ── Loading indicator ─────────────────────────────────────────────
        let status_el: Option<gpui::AnyElement> = if is_loading {
            Some(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child("Thinking…")
                    .into_any_element(),
            )
        } else {
            None
        };

        // ── Submit + Cancel buttons ────────────────────────────────────────
        let submit_entity = this_entity.clone();
        let submit_btn = Button::new(gpui::ElementId::Name("palette-submit".into()))
            .label("Submit")
            .on_click(move |_: &ClickEvent, _window, cx| {
                submit_entity.update(cx, |palette, cx| {
                    palette.submit(cx);
                });
            });

        let cancel_entity = this_entity.clone();
        let cancel_btn = Button::new(gpui::ElementId::Name("palette-cancel".into()))
            .ghost()
            .label("Cancel")
            .on_click(move |_: &ClickEvent, _window, cx| {
                cancel_entity.update(cx, |palette, cx| {
                    palette.close(cx);
                });
            });

        // ── Modal card ────────────────────────────────────────────────────
        let mut card = v_flex()
            .w(gpui::px(480.0))
            .gap_y_2()
            .p_4()
            .rounded(cx.theme().radius)
            .border_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .shadow_md()
            .child(
                div()
                    .text_sm()
                    .font_semibold()
                    .text_color(cx.theme().foreground)
                    .child("Command Palette"),
            )
            .child(Input::new(&input_entity));

        if !action_items.is_empty() {
            card = card.child(v_flex().w_full().gap_y_0p5().children(action_items));
        }

        if let Some(err) = error_el {
            card = card.child(err);
        }

        if let Some(status) = status_el {
            card = card.child(status);
        }

        card = card.child(
            h_flex()
                .w_full()
                .gap_x_2()
                .justify_end()
                .child(cancel_btn)
                .child(submit_btn),
        );

        // ── Scrim + centered card ─────────────────────────────────────────
        outer
            .flex()
            .items_center()
            .justify_center()
            .bg(cx.theme().background.opacity(0.75))
            .child(card)
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::filter_actions;

    #[test]
    fn filter_actions_empty_query_returns_all() -> anyhow::Result<()> {
        let actions = &[
            "New agent",
            "Stop agent",
            "Toggle render mode",
            "Focus next",
        ];
        let result = filter_actions(actions, "");
        assert_eq!(result.len(), 4);
        Ok(())
    }

    #[test]
    fn filter_actions_case_insensitive_substring() -> anyhow::Result<()> {
        let actions = &[
            "New agent",
            "Stop agent",
            "Toggle render mode",
            "Focus next",
        ];
        let result = filter_actions(actions, "AGE");
        // "New agent" and "Stop agent" both contain "age"
        assert_eq!(result.len(), 2);
        assert!(result.iter().any(|s| s == "New agent"));
        assert!(result.iter().any(|s| s == "Stop agent"));
        Ok(())
    }

    #[test]
    fn filter_actions_no_match_returns_empty() -> anyhow::Result<()> {
        let actions = &["New agent", "Stop agent"];
        let result = filter_actions(actions, "xyzzy");
        assert!(result.is_empty());
        Ok(())
    }

    #[test]
    fn filter_actions_exact_match() -> anyhow::Result<()> {
        let actions = &[
            "New agent",
            "Stop agent",
            "Toggle render mode",
            "Focus next",
        ];
        let result = filter_actions(actions, "Focus next");
        assert_eq!(result.len(), 1);
        assert_eq!(result[0], "Focus next");
        Ok(())
    }
}
