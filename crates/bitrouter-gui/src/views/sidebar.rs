//! [`SidebarView`] — grouped agent list with focus and multi-select.
//!
//! The sidebar uses plain `div()` flex primitives for session rows, giving us
//! full control over layout and click handling.

use bitrouter_gui_core::{
    protocol::{SessionId, SessionStatus, TabId},
    state::SessionView,
};
use gpui::{
    div, prelude::FluentBuilder as _, ClickEvent, Context, Entity, InteractiveElement, IntoElement,
    ParentElement, Render, StatefulInteractiveElement, Styled, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, StyledExt,
};

use crate::app_model::AppModel;

// ── helpers ──────────────────────────────────────────────────────────────────

/// Return a short status glyph for a session status.
pub fn status_glyph(status: SessionStatus) -> &'static str {
    match status {
        SessionStatus::Running => "▮",
        SessionStatus::WaitingPermission => "⚠",
        SessionStatus::Exited => "✓",
        SessionStatus::Errored => "✗",
        SessionStatus::Spawning => "◐",
        SessionStatus::Idle => "·",
    }
}

/// Group a slice of session views by their `tab` field, preserving first-seen
/// order of both tabs and sessions within each tab.
pub fn group_sessions(sessions: &[SessionView]) -> Vec<(TabId, Vec<&SessionView>)> {
    let mut order: Vec<TabId> = Vec::new();
    let mut map: std::collections::HashMap<TabId, Vec<&SessionView>> =
        std::collections::HashMap::new();

    for sv in sessions {
        let tab = sv.session.tab.clone();
        if !map.contains_key(&tab) {
            order.push(tab.clone());
            map.insert(tab.clone(), Vec::new());
        }
        if let Some(group) = map.get_mut(&tab) {
            group.push(sv);
        }
    }

    order
        .into_iter()
        .filter_map(|tab| map.remove(&tab).map(|views| (tab, views)))
        .collect()
}

/// Format a micro-USD cost as `$X.XX`.
fn format_cost(micro_usd: u64) -> String {
    let dollars = micro_usd / 1_000_000;
    let cents = (micro_usd % 1_000_000) / 10_000;
    format!("${dollars}.{cents:02}")
}

// ── row data ─────────────────────────────────────────────────────────────────

/// Snapshot of the data needed to render one session row — pre-extracted so we
/// don't hold a borrow on `cx` while building the element tree.
struct RowData {
    id: SessionId,
    label: String,
    sub_label: String,
    is_focused: bool,
}

// ── view ─────────────────────────────────────────────────────────────────────

/// Sidebar view: shows all sessions grouped by tab.
///
/// Regular click → `set_focus`; ⌘-click (Cmd) → `toggle_selection`.
/// A "+ new agent" button stub appears at the bottom of each group.
pub struct SidebarView {
    model: Entity<AppModel>,
}

impl SidebarView {
    /// Create a new [`SidebarView`] backed by `model`.
    ///
    /// Observes `model` so the view re-renders whenever the backing entity
    /// is updated by the feed pump.
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self { model }
    }
}

impl Render for SidebarView {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        // Read all state data while we have the borrow, then release it before
        // building the element tree (which requires mutable cx access).
        let (focus, selection, groups_data) = {
            let state = &self.model.read(cx).state;
            let focus = state.focus.clone();
            let selection = state.selection.clone();
            // Pre-extract everything needed for rendering to avoid holding the borrow.
            let groups: Vec<(TabId, Vec<RowData>)> = group_sessions(&state.sessions)
                .into_iter()
                .map(|(tab, views)| {
                    let rows = views
                        .into_iter()
                        .map(|sv| {
                            let is_focused = focus.as_ref().is_some_and(|f| f == &sv.session.id);
                            let is_selected = selection.iter().any(|s| s == &sv.session.id);
                            let glyph = status_glyph(sv.session.status);
                            let sel_marker = if is_selected { " ✔" } else { "" };
                            RowData {
                                id: sv.session.id.clone(),
                                label: format!("{} {}{}", glyph, sv.session.name, sel_marker),
                                sub_label: format!(
                                    "{} · {}  {}",
                                    sv.session.harness,
                                    sv.session.model,
                                    format_cost(sv.cost_micro_usd),
                                ),
                                is_focused,
                            }
                        })
                        .collect();
                    (tab, rows)
                })
                .collect();
            (focus, selection, groups)
        };
        // cx borrow released here.

        let model = self.model.clone();

        let group_elements: Vec<_> = groups_data
            .into_iter()
            .map(|(tab, rows)| {
                let tab_label = tab.0.clone();

                let row_elements: Vec<_> = rows
                    .into_iter()
                    .map(|row| {
                        let id_click = row.id.clone();
                        let id_cmd = row.id.clone();
                        let m1 = model.clone();
                        let m2 = model.clone();

                        h_flex()
                            .id(gpui::ElementId::Name(
                                format!("session-row-{}", row.id.0).into(),
                            ))
                            .w_full()
                            .px_2()
                            .py_1()
                            .gap_x_2()
                            .rounded(cx.theme().radius)
                            .cursor_pointer()
                            .when(row.is_focused, |this| {
                                this.bg(cx.theme().tokens.sidebar_accent)
                                    .text_color(cx.theme().sidebar_accent_foreground)
                            })
                            .when(!row.is_focused, |this| {
                                this.hover(|this| this.bg(cx.theme().sidebar_accent.opacity(0.5)))
                            })
                            .child(
                                v_flex()
                                    .flex_1()
                                    .gap_0()
                                    .child(div().text_sm().child(row.label))
                                    .child(
                                        div()
                                            .text_xs()
                                            .text_color(cx.theme().muted_foreground)
                                            .child(row.sub_label),
                                    ),
                            )
                            .on_click(move |event: &ClickEvent, _window, cx| {
                                if event.modifiers().platform {
                                    // ⌘-click → toggle selection
                                    let id = id_cmd.clone();
                                    m2.update(cx, move |m, cx| {
                                        m.toggle_selection(&id);
                                        cx.notify();
                                    });
                                } else {
                                    // regular click → set focus
                                    let id = id_click.clone();
                                    m1.update(cx, move |m, cx| {
                                        m.set_focus(&id);
                                        cx.notify();
                                    });
                                }
                            })
                            .into_any_element()
                    })
                    .collect();

                let new_agent_btn = Button::new(gpui::ElementId::Name(
                    format!("new-agent-{tab_label}").into(),
                ))
                .ghost()
                .label("+ new agent")
                .on_click(|_: &ClickEvent, _window, _cx| {
                    // TODO(task 2.10): dispatch SpawnAgent command
                });

                v_flex()
                    .w_full()
                    .gap_y_0p5()
                    .child(
                        h_flex()
                            .px_2()
                            .py_1()
                            .text_xs()
                            .font_semibold()
                            .text_color(cx.theme().muted_foreground)
                            .child(div().child(tab_label)),
                    )
                    .children(row_elements)
                    .child(h_flex().px_2().child(new_agent_btn))
                    .into_any_element()
            })
            .collect();

        // Suppress unused-variable warnings in tests where focus/selection are
        // computed but only used inside closures.
        let _ = focus;
        let _ = selection;

        v_flex()
            .w_full()
            .h_full()
            .gap_y_1()
            .overflow_hidden()
            .children(group_elements)
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{group_sessions, status_glyph};
    use bitrouter_gui_core::{
        protocol::{RenderMode, Session, SessionId, SessionStatus, TabId},
        state::SessionView,
    };

    fn make_session_view(id: &str, tab: &str) -> SessionView {
        SessionView {
            session: Session {
                id: SessionId(id.into()),
                name: id.into(),
                tab: TabId(tab.into()),
                harness: "claude-code".into(),
                model: "claude".into(),
                status: SessionStatus::Running,
                render_mode: RenderMode::Terminal,
            },
            transcript: Vec::new(),
            pending: None,
            cost_micro_usd: 0,
            tokens_in: 0,
            tokens_out: 0,
            last_route: None,
            failovers: 0,
            latencies_ms: Vec::new(),
        }
    }

    #[test]
    fn status_glyph_all_variants() -> anyhow::Result<()> {
        assert_eq!(status_glyph(SessionStatus::Running), "▮");
        assert_eq!(status_glyph(SessionStatus::WaitingPermission), "⚠");
        assert_eq!(status_glyph(SessionStatus::Exited), "✓");
        assert_eq!(status_glyph(SessionStatus::Errored), "✗");
        assert_eq!(status_glyph(SessionStatus::Spawning), "◐");
        assert_eq!(status_glyph(SessionStatus::Idle), "·");
        Ok(())
    }

    #[test]
    fn group_sessions_scenario_gives_one_group_of_three() -> anyhow::Result<()> {
        // Build state from the mock scenario via synchronous block_on drain.
        use bitrouter_gui_core::{feed::Feed as _, state};
        use futures::executor::block_on;

        let mut feed_handle = bitrouter_gui_core::feed::MockFeed::scenario().connect();
        let mut st = state::State::default();

        block_on(async {
            loop {
                // poll_fn lets us drain without blocking indefinitely.
                let maybe = futures::future::poll_fn(|ctx| {
                    use std::task::Poll;
                    match futures::StreamExt::poll_next_unpin(&mut feed_handle.events, ctx) {
                        Poll::Ready(v) => Poll::Ready(v),
                        Poll::Pending => Poll::Ready(None),
                    }
                })
                .await;
                match maybe {
                    Some(ev) => state::reduce(&mut st, ev),
                    None => break,
                }
            }
        });

        let groups = group_sessions(&st.sessions);
        assert_eq!(groups.len(), 1, "expected exactly 1 tab group");
        let (tab, sessions) = &groups[0];
        assert_eq!(tab.0, "auth-feature");
        assert_eq!(sessions.len(), 3, "expected 3 sessions in auth-feature");
        Ok(())
    }

    #[test]
    fn group_sessions_preserves_insertion_order() -> anyhow::Result<()> {
        let sessions = vec![
            make_session_view("a1", "alpha"),
            make_session_view("b1", "beta"),
            make_session_view("a2", "alpha"),
            make_session_view("b2", "beta"),
        ];
        let groups = group_sessions(&sessions);
        assert_eq!(groups.len(), 2);
        assert_eq!(groups[0].0 .0, "alpha");
        assert_eq!(groups[1].0 .0, "beta");
        assert_eq!(groups[0].1.len(), 2);
        assert_eq!(groups[1].1.len(), 2);
        Ok(())
    }

    /// View construction: build SidebarView over AppModel and park — no panic.
    #[gpui::test]
    fn sidebar_renders_without_panic(cx: &mut gpui::TestAppContext) {
        use crate::app_model::AppModel;
        use bitrouter_gui_core::feed::MockFeed;
        use gpui::AppContext as _;

        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        cx.update(|cx| {
            let _ = cx.new(|cx| super::SidebarView::new(model.clone(), cx));
        });
    }
}
