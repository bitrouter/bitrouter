//! [`Transcript`] — renders the ACP session transcript.
//!
//! Displays each [`TranscriptItem`] in order:
//! - `Message` → normal text block
//! - `Thought` → dim, italic (muted_foreground)
//! - `ToolCall` → title + status glyph, optional diff block
//!
//! [`PermissionModal`] — overlay dialog for pending permission requests.

use bitrouter_gui_core::{
    protocol::ToolStatus,
    state::{SessionView, TranscriptItem},
};
use gpui::{div, prelude::FluentBuilder as _, IntoElement, ParentElement, Styled};
use gpui_component::{scroll::ScrollableElement, v_flex, ActiveTheme, StyledExt};

// ── helpers ──────────────────────────────────────────────────────────────────

/// Return a short status glyph for a tool call status.
pub fn tool_status_glyph(status: ToolStatus) -> &'static str {
    match status {
        ToolStatus::Pending => "○",
        ToolStatus::Running => "◐",
        ToolStatus::Ok => "✓",
        ToolStatus::Failed => "✗",
    }
}

// ── transcript render helper ─────────────────────────────────────────────────

/// Render the transcript for the given `SessionView`.
///
/// This is a free function (not a view entity) so it can be called from any
/// parent render context without requiring its own `Entity`.
pub fn render_transcript<'a>(
    sv: &'a SessionView,
    cx: &'a mut gpui::Context<impl gpui::Render>,
) -> impl IntoElement + 'a {
    let items: Vec<_> = sv
        .transcript
        .iter()
        .map(|item| match item {
            TranscriptItem::Message { text } => div()
                .w_full()
                .px_3()
                .py_1()
                .text_sm()
                .text_color(cx.theme().foreground)
                .child(text.clone())
                .into_any_element(),

            TranscriptItem::Thought { text } => div()
                .w_full()
                .px_3()
                .py_1()
                .text_sm()
                .italic()
                .text_color(cx.theme().muted_foreground)
                .child(text.clone())
                .into_any_element(),

            TranscriptItem::ToolCall {
                title,
                status,
                diff,
            } => {
                let glyph = tool_status_glyph(*status);
                let title_row = div()
                    .w_full()
                    .px_3()
                    .py_1()
                    .text_sm()
                    .font_semibold()
                    .text_color(cx.theme().foreground)
                    .child(format!("{glyph} {title}"));

                let diff_block = diff.as_ref().map(|d| {
                    div()
                        .w_full()
                        .mx_3()
                        .my_1()
                        .px_2()
                        .py_1()
                        .rounded(cx.theme().radius)
                        .border_1()
                        .border_color(cx.theme().border)
                        .bg(cx.theme().secondary)
                        .text_xs()
                        .font_family("monospace")
                        .text_color(cx.theme().muted_foreground)
                        .child(d.clone())
                });

                v_flex()
                    .w_full()
                    .child(title_row)
                    .when_some(diff_block, |el, block| el.child(block))
                    .into_any_element()
            }
        })
        .collect();

    v_flex()
        .w_full()
        .h_full()
        .overflow_y_scrollbar()
        .gap_y_1()
        .children(items)
}

// ── tests ────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::tool_status_glyph;
    use bitrouter_gui_core::protocol::ToolStatus;

    #[test]
    fn tool_status_glyph_all_variants() -> anyhow::Result<()> {
        assert_eq!(tool_status_glyph(ToolStatus::Pending), "○");
        assert_eq!(tool_status_glyph(ToolStatus::Running), "◐");
        assert_eq!(tool_status_glyph(ToolStatus::Ok), "✓");
        assert_eq!(tool_status_glyph(ToolStatus::Failed), "✗");
        Ok(())
    }
}
