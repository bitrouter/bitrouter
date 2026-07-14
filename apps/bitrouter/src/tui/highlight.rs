//! Syntax highlighting for fenced code blocks in ACP-event panes
//! (TUI_SPEC §8b): syntect + two-face, foreground colors only — no italic or
//! underline — and capped by line length so a pathological line can't stall a
//! frame. Highlighting is per-line (fresh parse state per line): monitors
//! trade multi-line constructs for zero cross-frame state.

use std::sync::OnceLock;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::Span;
use syntect::easy::HighlightLines;
use syntect::highlighting::{FontStyle, Theme};
use syntect::parsing::SyntaxSet;

/// Lines longer than this render unhighlighted (cap by size).
const MAX_HL_CHARS: usize = 400;

fn assets() -> &'static (SyntaxSet, Theme) {
    static ASSETS: OnceLock<(SyntaxSet, Theme)> = OnceLock::new();
    ASSETS.get_or_init(|| {
        let syntaxes = two_face::syntax::extra_newlines();
        let theme = two_face::theme::extra()
            .get(two_face::theme::EmbeddedThemeName::Base16OceanDark)
            .clone();
        (syntaxes, theme)
    })
}

/// Highlight one code line for `lang` into styled spans. Falls back to a
/// single dim plain span when colors are off, the language is unknown, or the
/// line exceeds the size cap.
pub fn spans(lang: &str, text: &str, no_color: bool) -> Vec<Span<'static>> {
    let plain = || {
        vec![Span::styled(
            text.to_string(),
            if no_color {
                Style::default()
            } else {
                Style::default().fg(Color::Gray)
            },
        )]
    };
    if no_color || text.len() > MAX_HL_CHARS {
        return plain();
    }
    let (syntaxes, theme) = assets();
    let Some(syntax) = syntaxes.find_syntax_by_token(lang) else {
        return plain();
    };
    let mut hl = HighlightLines::new(syntax, theme);
    // The "newlines" syntax set expects the terminator present.
    let line = format!("{text}\n");
    let Ok(regions) = hl.highlight_line(&line, syntaxes) else {
        return plain();
    };
    regions
        .into_iter()
        .filter_map(|(style, seg)| {
            let seg = seg.trim_end_matches('\n');
            if seg.is_empty() {
                return None;
            }
            let fg = style.foreground;
            let mut out = Style::default().fg(Color::Rgb(fg.r, fg.g, fg.b));
            // Bold survives; italic/underline are dropped by design.
            if style.font_style.contains(FontStyle::BOLD) {
                out = out.add_modifier(Modifier::BOLD);
            }
            Some(Span::styled(seg.to_string(), out))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rust_keyword_gets_a_foreground_color() {
        let spans = spans("rust", "fn main() {}", false);
        assert!(
            spans
                .iter()
                .any(|s| matches!(s.style.fg, Some(Color::Rgb(..)))),
            "highlighted code carries RGB foregrounds: {spans:?}"
        );
        let text: String = spans.iter().map(|s| s.content.as_ref()).collect();
        assert_eq!(text, "fn main() {}", "content survives highlighting");
    }

    #[test]
    fn no_color_and_unknown_lang_fall_back_to_plain() {
        for (lang, nc) in [("rust", true), ("zzz-not-a-lang", false)] {
            let spans = spans(lang, "fn main() {}", nc);
            assert_eq!(spans.len(), 1, "single plain span for {lang}/{nc}");
            assert!(!matches!(spans[0].style.fg, Some(Color::Rgb(..))));
        }
    }

    #[test]
    fn oversized_line_is_not_highlighted() {
        let long = "x".repeat(MAX_HL_CHARS + 1);
        let spans = spans("rust", &long, false);
        assert_eq!(spans.len(), 1);
    }

    #[test]
    fn no_italic_or_underline_ever() {
        // Markdown italics are the classic case that would set italic.
        let spans = spans("md", "*emphasis* and _more_", false);
        for s in &spans {
            assert!(
                !s.style
                    .add_modifier
                    .intersects(Modifier::ITALIC | Modifier::UNDERLINED),
                "italic/underline must be dropped: {s:?}"
            );
        }
    }
}
