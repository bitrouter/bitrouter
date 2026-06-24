//! [`Root`] — top-level shell: title bar + sidebar + center + status bar.
//!
//! Also handles global gpui actions from `keymap`:
//! - `OpenPalette` — open the command palette
//! - `FocusSession { n }` — focus the Nth session (1-indexed)

use gpui::{
    div, px, AppContext, ClickEvent, Context, Entity, InteractiveElement as _, IntoElement,
    ParentElement, Render, Styled, Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, StyledExt,
};

use crate::{
    app_model::AppModel,
    keymap::{nth_session_id, FocusSession, OpenPalette},
    views::{
        broadcast::BroadcastBar, center::Center, command_palette::CommandPalette,
        dashboard::Dashboard, sidebar::SidebarView, status_bar::StatusBar,
    },
};

/// Format a micro-USD total cost as `$X.XX`.
///
/// This is a pure function exposed so it can be unit-tested independently.
pub fn format_cost(micro_usd: u64) -> String {
    let dollars = micro_usd / 1_000_000;
    let cents = (micro_usd % 1_000_000) / 10_000;
    format!("${dollars}.{cents:02}")
}

/// Which view is shown in the main content area.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RootView {
    Agents,
    Dashboard,
}

/// Root shell view: title bar at top, sidebar + center in the middle, status bar at bottom.
pub struct Root {
    model: Entity<AppModel>,
    sidebar: Entity<SidebarView>,
    center: Entity<Center>,
    status_bar: Entity<StatusBar>,
    command_palette: Entity<CommandPalette>,
    broadcast_bar: Entity<BroadcastBar>,
    dashboard: Entity<Dashboard>,
    active_view: RootView,
}

impl Root {
    /// Construct the root shell backed by `model`.
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|_cx| SidebarView::new(model.clone()));
        let center = cx.new(|_cx| Center::new(model.clone()));
        let status_bar = cx.new(|_cx| StatusBar::new(model.clone()));
        let command_palette = cx.new(|_cx| CommandPalette::new(model.clone()));
        let broadcast_bar = cx.new(|_cx| BroadcastBar::new(model.clone()));
        let dashboard = cx.new(|_cx| Dashboard::new(model.clone()));
        Self {
            model,
            sidebar,
            center,
            status_bar,
            command_palette,
            broadcast_bar,
            dashboard,
            active_view: RootView::Agents,
        }
    }

    /// Handle `OpenPalette` action — opens the command palette.
    fn handle_open_palette(
        &mut self,
        _: &OpenPalette,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        self.command_palette.update(cx, |palette, cx| {
            palette.open(cx);
        });
    }

    /// Handle `FocusSession { n }` action — focus the Nth session.
    fn handle_focus_session(
        &mut self,
        action: &FocusSession,
        _window: &mut Window,
        cx: &mut Context<Self>,
    ) {
        let n = action.n;
        let ids: Vec<bitrouter_gui_core::protocol::SessionId> = self
            .model
            .read(cx)
            .state
            .sessions
            .iter()
            .map(|v| v.session.id.clone())
            .collect();

        if let Some(id) = nth_session_id(&ids, n) {
            self.model.update(cx, |m, cx| {
                m.set_focus(&id);
                cx.notify();
            });
        }
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cost = self.model.read(cx).state.session_cost_micro_usd();
        let cost_label = format!("session {}", format_cost(cost));
        let active_view = self.active_view;

        // ⌘K button — remains as fallback trigger even with real key binding.
        let palette_entity = self.command_palette.clone();
        let open_palette_btn = Button::new(gpui::ElementId::Name("open-palette".into()))
            .ghost()
            .label("⌘K")
            .on_click(move |_: &ClickEvent, _window, cx| {
                palette_entity.update(cx, |palette, cx| {
                    palette.open(cx);
                });
            });

        // Dashboard / Agents toggle nav button.
        let this_entity = cx.entity().clone();
        let (nav_label, nav_target) = match active_view {
            RootView::Agents => ("Dashboard", RootView::Dashboard),
            RootView::Dashboard => ("Agents", RootView::Agents),
        };
        let nav_btn = Button::new(gpui::ElementId::Name("nav-toggle".into()))
            .ghost()
            .label(nav_label)
            .on_click(move |_: &ClickEvent, _window, cx| {
                this_entity.update(cx, |root, cx| {
                    root.active_view = nav_target;
                    cx.notify();
                });
            });

        // Title bar
        let title_bar = h_flex()
            .w_full()
            .h_8()
            .px_3()
            .border_b_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .items_center()
            .child(
                div()
                    .font_semibold()
                    .text_sm()
                    .text_color(cx.theme().foreground)
                    .child("BitRouter"),
            )
            .child(div().flex_1())
            .child(nav_btn)
            .child(open_palette_btn)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(cost_label),
            );

        // Main content: sidebar + (center or dashboard)
        let main_content: gpui::AnyElement = match active_view {
            RootView::Agents => h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    div()
                        .w(px(240.0))
                        .h_full()
                        .flex_shrink_0()
                        .border_r_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().secondary)
                        .child(self.sidebar.clone()),
                )
                .child(self.center.clone())
                .into_any_element(),
            RootView::Dashboard => h_flex()
                .flex_1()
                .min_h_0()
                .w_full()
                .child(
                    div()
                        .w(px(240.0))
                        .h_full()
                        .flex_shrink_0()
                        .border_r_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().secondary)
                        .child(self.sidebar.clone()),
                )
                .child(div().flex_1().h_full().child(self.dashboard.clone()))
                .into_any_element(),
        };

        // Full-window column with palette overlay on top.
        // `.on_action` handlers are registered here so they receive events bubbled
        // from the entire window focus chain.
        div()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .on_action(cx.listener(Self::handle_open_palette))
            .on_action(cx.listener(Self::handle_focus_session))
            .child(
                v_flex()
                    .size_full()
                    .child(title_bar)
                    .child(main_content)
                    .child(self.broadcast_bar.clone())
                    .child(self.status_bar.clone()),
            )
            .child(self.command_palette.clone())
    }
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::{format_cost, Root};
    use crate::app_model::AppModel;
    use bitrouter_gui_core::feed::MockFeed;
    use gpui::{AppContext as _, TestAppContext};

    #[test]
    fn format_cost_zero() -> anyhow::Result<()> {
        assert_eq!(format_cost(0), "$0.00");
        Ok(())
    }

    #[test]
    fn format_cost_one_dollar_68_cents() -> anyhow::Result<()> {
        // 1_680_000 micro-USD = $1.68
        assert_eq!(format_cost(1_680_000), "$1.68");
        Ok(())
    }

    #[test]
    fn format_cost_scenario_total() -> anyhow::Result<()> {
        // Scenario: auth-fix $0.42 + refactor-api $1.10 = $1.52
        let total = 420_000u64 + 1_100_000u64;
        assert_eq!(format_cost(total), "$1.52");
        Ok(())
    }

    #[test]
    fn format_cost_large() -> anyhow::Result<()> {
        assert_eq!(format_cost(10_000_000), "$10.00");
        Ok(())
    }

    /// View construction: build Root over AppModel and run_until_parked — no panic.
    #[gpui::test]
    fn root_renders_without_panic(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        cx.update(|cx| {
            let _ = cx.new(|cx| Root::new(model.clone(), cx));
        });
    }

    /// Cost label includes the formatted total after the scenario events.
    #[gpui::test]
    fn root_cost_reflects_model_state(cx: &mut TestAppContext) {
        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        // The scenario emits RequestCompleted for auth-fix ($0.42) and refactor-api ($1.10).
        let cost = model.read_with(cx, |m, _| m.state.session_cost_micro_usd());
        assert_eq!(format_cost(cost), "$1.52");
    }

    /// Root starts in Agents view.
    #[gpui::test]
    fn root_starts_in_agents_view(cx: &mut TestAppContext) {
        use super::RootView;

        let model = cx.update(|cx| cx.new(|cx| AppModel::new(MockFeed::scenario(), cx)));
        cx.run_until_parked();

        let active_view = cx.update(|cx| {
            let root = cx.new(|cx| Root::new(model.clone(), cx));
            root.read(cx).active_view
        });
        assert_eq!(active_view, RootView::Agents);
    }
}
