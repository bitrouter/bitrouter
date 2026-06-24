//! [`StatusBar`] — bottom HUD strip showing route, token counts, failovers, p50.

use bitrouter_gui_core::state::Hud;
use gpui::{div, Context, Entity, IntoElement, ParentElement, Render, Styled, Window};
use gpui_component::{h_flex, ActiveTheme};

use crate::app_model::AppModel;

/// Bottom status bar that reads the HUD from [`AppModel`] and renders it as a
/// single horizontal strip.
pub struct StatusBar {
    model: Entity<AppModel>,
}

impl StatusBar {
    /// Create a new [`StatusBar`] backed by `model`.
    ///
    /// Observes `model` so the view re-renders whenever the backing entity
    /// is updated by the feed pump.
    pub fn new(model: Entity<AppModel>, cx: &mut Context<Self>) -> Self {
        cx.observe(&model, |_, _, cx| cx.notify()).detach();
        Self { model }
    }
}

impl Render for StatusBar {
    fn render(&mut self, _window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        let hud: Hud = self.model.read(cx).state.hud();
        let text = hud_text(&hud);

        h_flex()
            .w_full()
            .h_6()
            .px_3()
            .border_t_1()
            .border_color(cx.theme().border)
            .bg(cx.theme().background)
            .text_color(cx.theme().muted_foreground)
            .text_xs()
            .child(div().flex_1().child(text))
    }
}

/// Format a [`Hud`] snapshot into a single human-readable line.
///
/// Example: `route claude→qwen (cost-gate) · tok 8k↑ 2k↓ · failovers 0 · p50 1.1s`
pub fn hud_text(hud: &Hud) -> String {
    let mut parts: Vec<String> = Vec::new();

    if let Some(route) = &hud.last_route {
        parts.push(format!(
            "route {}→{} ({})",
            route.asked, route.routed, route.rule
        ));
    }

    let tok_in = format_tokens(hud.tokens_in);
    let tok_out = format_tokens(hud.tokens_out);
    parts.push(format!("tok {tok_in}↑ {tok_out}↓"));

    parts.push(format!("failovers {}", hud.failovers));

    match hud.p50_ms {
        Some(ms) => parts.push(format!("p50 {:.1}s", ms as f64 / 1000.0)),
        None => parts.push("p50 —".to_string()),
    }

    parts.join(" · ")
}

/// Format a token count as a compact human-readable string (e.g. `8k`, `1.2M`).
fn format_tokens(count: u64) -> String {
    if count >= 1_000_000 {
        format!("{:.1}M", count as f64 / 1_000_000.0)
    } else if count >= 1_000 {
        format!("{}k", count / 1_000)
    } else {
        count.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::{format_tokens, hud_text};
    use bitrouter_gui_core::{protocol::Route, state::Hud};

    #[test]
    fn hud_text_with_route() -> anyhow::Result<()> {
        let hud = Hud {
            tokens_in: 8_000,
            tokens_out: 2_000,
            failovers: 0,
            p50_ms: Some(1_100),
            last_route: Some(Route {
                asked: "claude".into(),
                routed: "qwen".into(),
                rule: "cost-gate".into(),
            }),
        };
        let text = hud_text(&hud);
        assert!(
            text.contains("route claude→qwen (cost-gate)"),
            "got: {text}"
        );
        assert!(text.contains("tok 8k↑ 2k↓"), "got: {text}");
        assert!(text.contains("failovers 0"), "got: {text}");
        assert!(text.contains("p50 1.1s"), "got: {text}");
        Ok(())
    }

    #[test]
    fn hud_text_without_route() -> anyhow::Result<()> {
        let hud = Hud {
            tokens_in: 0,
            tokens_out: 0,
            failovers: 0,
            p50_ms: None,
            last_route: None,
        };
        let text = hud_text(&hud);
        assert!(!text.contains("route"), "no route expected, got: {text}");
        assert!(text.contains("p50 —"), "got: {text}");
        Ok(())
    }

    #[test]
    fn format_tokens_ranges() -> anyhow::Result<()> {
        assert_eq!(format_tokens(0), "0");
        assert_eq!(format_tokens(500), "500");
        assert_eq!(format_tokens(1_000), "1k");
        assert_eq!(format_tokens(8_000), "8k");
        assert_eq!(format_tokens(1_500_000), "1.5M");
        Ok(())
    }
}
