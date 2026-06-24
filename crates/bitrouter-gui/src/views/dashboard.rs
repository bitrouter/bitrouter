//! [`Dashboard`] — per-model cost table and spend bar chart.
//!
//! Shows aggregate cost grouped by model (summed across all sessions) as both
//! a text table and a proportional bar chart. Helps the user understand which
//! model is consuming the most budget at a glance.

use bitrouter_gui_core::state::SessionView;
use gpui::{div, relative, Context, Entity, IntoElement, ParentElement, Render, Styled, Window};
use gpui_component::{h_flex, v_flex, ActiveTheme, StyledExt};

use crate::{app_model::AppModel, views::root::format_cost};

// ── Pure helper ────────────────────────────────────────────────────────────────

/// Aggregate `cost_micro_usd` from `sessions` grouped by `session.model`.
///
/// Returns a `Vec<(model_name, total_cost_micro_usd)>` sorted by cost
/// descending; ties are broken by first-seen order (i.e. the model that
/// appeared earlier in `sessions` comes first).
pub fn cost_by_model(sessions: &[SessionView]) -> Vec<(String, u64)> {
    // Use a Vec to track insertion order + an index map for O(n) lookup.
    let mut order: Vec<String> = Vec::new();
    let mut totals: std::collections::HashMap<String, u64> = std::collections::HashMap::new();

    for sv in sessions {
        let model = sv.session.model.clone();
        if !totals.contains_key(&model) {
            order.push(model.clone());
            totals.insert(model.clone(), 0);
        }
        if let Some(acc) = totals.get_mut(&model) {
            *acc = acc.saturating_add(sv.cost_micro_usd);
        }
    }

    // Build result in insertion order first, then stable-sort descending by cost
    // so that equal costs keep first-seen order.
    let mut result: Vec<(String, u64)> = order
        .into_iter()
        .filter_map(|m| totals.remove(&m).map(|c| (m, c)))
        .collect();

    // Sort descending by cost; equal costs keep first-seen order (stable_sort).
    result.sort_by_key(|(_, cost)| std::cmp::Reverse(*cost));
    result
}

// ── View ──────────────────────────────────────────────────────────────────────

/// Dashboard view: per-model cost table + spend bar chart.
pub struct Dashboard {
    model: Entity<AppModel>,
}

impl Dashboard {
    /// Create a new [`Dashboard`] backed by `model`.
    ///
    /// Observes `model` so the view re-renders whenever the backing entity
    /// is updated by the feed pump.
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self { model }
    }
}

impl Render for Dashboard {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let rows = {
            let state = &self.model.read(cx).state;
            cost_by_model(&state.sessions)
        };

        // Max cost for bar chart scaling (avoid divide-by-zero).
        let max_cost = rows.iter().map(|(_, c)| *c).max().unwrap_or(1).max(1);

        // ── Table header ──────────────────────────────────────────────────────
        let table_header = h_flex()
            .w_full()
            .px_3()
            .py_1()
            .border_b_1()
            .border_color(cx.theme().border)
            .child(
                div()
                    .flex_1()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child("Model"),
            )
            .child(
                div()
                    .w_24()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child("Sessions"),
            )
            .child(
                div()
                    .w_24()
                    .text_xs()
                    .font_semibold()
                    .text_color(cx.theme().muted_foreground)
                    .child("Total cost"),
            );

        // Count sessions per model (needed for the table column).
        let session_counts: std::collections::HashMap<String, usize> = {
            let state = &self.model.read(cx).state;
            let mut counts = std::collections::HashMap::new();
            for sv in &state.sessions {
                *counts.entry(sv.session.model.clone()).or_insert(0) += 1;
            }
            counts
        };

        // ── Table rows + bar chart rows ────────────────────────────────────
        let row_elements: Vec<gpui::AnyElement> = rows
            .iter()
            .map(|(model_name, cost)| {
                let count = session_counts.get(model_name).copied().unwrap_or(0);
                let cost_label = format_cost(*cost);
                // Bar width as a percentage of the container, 4px minimum for
                // non-zero costs so the bar is always visible.
                // Bar fraction in [0.0, 1.0]; minimum 0.04 so non-zero bars
                // are always visible.
                let bar_fraction = if max_cost > 0 && *cost > 0 {
                    (*cost as f32 / max_cost as f32).max(0.04)
                } else {
                    0.0_f32
                };

                v_flex()
                    .w_full()
                    .gap_y_0p5()
                    // Table row
                    .child(
                        h_flex()
                            .w_full()
                            .px_3()
                            .py_1()
                            .child(
                                div()
                                    .flex_1()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(model_name.clone()),
                            )
                            .child(
                                div()
                                    .w_24()
                                    .text_sm()
                                    .text_color(cx.theme().muted_foreground)
                                    .child(format!("{count}")),
                            )
                            .child(
                                div()
                                    .w_24()
                                    .text_sm()
                                    .text_color(cx.theme().foreground)
                                    .child(cost_label),
                            ),
                    )
                    // Bar chart row: a track div containing a filled inner div
                    // whose width is set via relative() proportional to max cost.
                    .child(
                        h_flex().w_full().px_3().pb_2().child(
                            div()
                                .h(gpui::px(6.0))
                                .w_full()
                                .rounded_full()
                                .bg(cx.theme().muted.opacity(0.4))
                                .child(
                                    div()
                                        .h_full()
                                        .rounded_full()
                                        .bg(cx.theme().primary)
                                        .w(relative(bar_fraction)),
                                ),
                        ),
                    )
                    .into_any_element()
            })
            .collect();

        // ── Empty state ───────────────────────────────────────────────────────
        let body: gpui::AnyElement = if row_elements.is_empty() {
            div()
                .flex_1()
                .size_full()
                .items_center()
                .justify_center()
                .child(
                    div()
                        .text_sm()
                        .text_color(cx.theme().muted_foreground)
                        .child("No sessions yet"),
                )
                .into_any_element()
        } else {
            v_flex().w_full().children(row_elements).into_any_element()
        };

        // ── Full dashboard pane ───────────────────────────────────────────────
        v_flex()
            .flex_1()
            .size_full()
            .bg(cx.theme().background)
            .child(
                // Section title
                h_flex()
                    .w_full()
                    .px_3()
                    .py_2()
                    .border_b_1()
                    .border_color(cx.theme().border)
                    .child(
                        div()
                            .text_sm()
                            .font_semibold()
                            .text_color(cx.theme().foreground)
                            .child("Spend by model"),
                    ),
            )
            .child(table_header)
            .child(body)
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{cost_by_model, Dashboard};
    use bitrouter_gui_core::{
        feed::Feed as _,
        state::{self, State},
    };
    use futures::executor::block_on;

    /// Build state from MockFeed::scenario() synchronously via poll_fn drain.
    fn scenario_state() -> State {
        let mut handle = bitrouter_gui_core::feed::MockFeed::scenario().connect();
        let mut st = State::default();
        block_on(async {
            loop {
                let maybe = futures::future::poll_fn(|ctx| {
                    use futures::StreamExt as _;
                    use std::task::Poll;
                    match handle.events.poll_next_unpin(ctx) {
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
        st
    }

    /// cost_by_model groups scenario sessions correctly and sorts by cost desc.
    ///
    /// Scenario costs:
    ///   auth-fix       → claude-opus-4  → 420_000
    ///   refactor-api   → qwen           → 1_100_000
    ///   add-tests      → gemini-flash   → 0 (no RequestCompleted)
    ///
    /// Expected order: qwen (1_100_000) > claude-opus-4 (420_000) > gemini-flash (0)
    #[test]
    fn cost_by_model_scenario() -> anyhow::Result<()> {
        let st = scenario_state();
        let rows = cost_by_model(&st.sessions);

        assert_eq!(rows.len(), 3, "expected 3 model rows");

        // Check sorted order by cost descending.
        assert_eq!(rows[0].0, "qwen");
        assert_eq!(rows[0].1, 1_100_000);

        assert_eq!(rows[1].0, "claude-opus-4");
        assert_eq!(rows[1].1, 420_000);

        assert_eq!(rows[2].0, "gemini-flash");
        assert_eq!(rows[2].1, 0);

        Ok(())
    }

    /// cost_by_model returns empty vec when there are no sessions.
    #[test]
    fn cost_by_model_empty() -> anyhow::Result<()> {
        let rows = cost_by_model(&[]);
        assert!(rows.is_empty());
        Ok(())
    }

    /// cost_by_model accumulates costs for multiple sessions with the same model.
    #[test]
    fn cost_by_model_multiple_same_model() -> anyhow::Result<()> {
        use bitrouter_gui_core::{
            protocol::{RenderMode, Session, SessionId, SessionStatus, TabId},
            state::SessionView,
        };

        let mk_sv = |id: &str, model: &str, cost: u64| SessionView {
            session: Session {
                id: SessionId(id.into()),
                name: id.into(),
                tab: TabId("t".into()),
                harness: "h".into(),
                model: model.into(),
                status: SessionStatus::Running,
                render_mode: RenderMode::Terminal,
            },
            transcript: Vec::new(),
            pending: None,
            cost_micro_usd: cost,
            tokens_in: 0,
            tokens_out: 0,
            last_route: None,
            failovers: 0,
            latencies_ms: Vec::new(),
        };

        let sessions = vec![
            mk_sv("s1", "claude", 500_000),
            mk_sv("s2", "claude", 300_000),
            mk_sv("s3", "gpt4", 1_000_000),
        ];

        let rows = cost_by_model(&sessions);
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0].0, "gpt4");
        assert_eq!(rows[0].1, 1_000_000);
        assert_eq!(rows[1].0, "claude");
        assert_eq!(rows[1].1, 800_000);

        Ok(())
    }

    /// Dashboard constructs over MockFeed::scenario() state without panicking.
    #[gpui::test]
    fn dashboard_renders_without_panic(cx: &mut gpui::TestAppContext) {
        use crate::app_model::AppModel;
        use bitrouter_gui_core::feed::MockFeed;
        use gpui::AppContext as _;

        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        cx.update(|cx| {
            let _ = cx.new(|cx| Dashboard::new(model.clone(), cx));
        });
    }
}
