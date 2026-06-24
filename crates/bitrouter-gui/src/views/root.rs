//! [`Root`] — top-level shell: title bar + sidebar + center + status bar.

use gpui::{
    div, px, AppContext, ClickEvent, Context, Entity, IntoElement, ParentElement, Render, Styled,
    Window,
};
use gpui_component::{
    button::{Button, ButtonVariants},
    h_flex, v_flex, ActiveTheme, StyledExt,
};

use crate::{
    app_model::AppModel,
    views::{
        center::Center, command_palette::CommandPalette, sidebar::SidebarView,
        status_bar::StatusBar,
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

/// Root shell view: title bar at top, sidebar + center in the middle, status bar at bottom.
pub struct Root {
    model: Entity<AppModel>,
    sidebar: Entity<SidebarView>,
    center: Entity<Center>,
    status_bar: Entity<StatusBar>,
    command_palette: Entity<CommandPalette>,
}

impl Root {
    /// Construct the root shell backed by `model`.
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        let sidebar = cx.new(|_cx| SidebarView::new(model.clone()));
        let center = cx.new(|_cx| Center::new(model.clone()));
        let status_bar = cx.new(|_cx| StatusBar::new(model.clone()));
        let command_palette = cx.new(|_cx| CommandPalette::new(model.clone()));
        Self {
            model,
            sidebar,
            center,
            status_bar,
            command_palette,
        }
    }
}

impl Render for Root {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let cost = self.model.read(cx).state.session_cost_micro_usd();
        let cost_label = format!("session {}", format_cost(cost));

        // ⌘K button — temporary trigger; real key binding is task 2.12.
        let palette_entity = self.command_palette.clone();
        let open_palette_btn = Button::new(gpui::ElementId::Name("open-palette".into()))
            .ghost()
            .label("⌘K")
            .on_click(move |_: &ClickEvent, _window, cx| {
                palette_entity.update(cx, |palette, cx| {
                    palette.open(cx);
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
            .child(open_palette_btn)
            .child(
                div()
                    .text_xs()
                    .text_color(cx.theme().muted_foreground)
                    .child(cost_label),
            );

        // Sidebar + center row
        let content_row = h_flex()
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
            .child(self.center.clone());

        // Full-window column with palette overlay on top
        div()
            .size_full()
            .relative()
            .bg(cx.theme().background)
            .child(
                v_flex()
                    .size_full()
                    .child(title_bar)
                    .child(content_row)
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
}
