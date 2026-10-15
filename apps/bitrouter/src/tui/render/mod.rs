//! Adapt shared transcript rows to terminal styles; keep source-reading anchors local.

pub mod markdown;

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

use crate::presentation::transcript::text::{Format, Row, Tone};

pub(crate) fn rows(rows: Vec<Row>) -> Vec<Line<'static>> {
    rows.into_iter()
        .map(|row| {
            Line::from(
                row.runs
                    .into_iter()
                    .map(|run| Span::styled(run.content, style(run.format)))
                    .collect::<Vec<_>>(),
            )
        })
        .collect()
}

fn style(format: Format) -> Style {
    let mut style = Style::default();
    if let Some(tone) = format.tone {
        style = style.fg(match tone {
            Tone::Text => Color::Reset,
            Tone::Action => Color::Cyan,
            Tone::Thought => Color::White,
            Tone::Muted => Color::DarkGray,
            Tone::Code => Color::Gray,
            Tone::Success => Color::Green,
            Tone::Pending => Color::Yellow,
            Tone::Error => Color::Red,
            Tone::Unknown => Color::Magenta,
        });
    }
    for (enabled, modifier) in [
        (format.bold, Modifier::BOLD),
        (format.italic, Modifier::ITALIC),
        (format.underlined, Modifier::UNDERLINED),
        (format.crossed_out, Modifier::CROSSED_OUT),
    ] {
        if enabled {
            style = style.add_modifier(modifier);
        }
    }
    style
}

/// Keep one row per source line for the Code journal's reading anchors.
pub fn source_message(message: &crate::presentation::journal::Message) -> Vec<Line<'static>> {
    let (prefix, style) = match message.voice {
        crate::presentation::journal::Voice::User => ("┃ ", Style::default().fg(Color::Cyan)),
        crate::presentation::journal::Voice::Agent => ("", Style::default()),
        crate::presentation::journal::Voice::Thought => ("· ", thought_style()),
    };
    message
        .text
        .split('\n')
        .map(|line| Line::from(Span::styled(format!("{prefix}{line}"), style)))
        .collect()
}

fn thought_style() -> Style {
    Style::default()
        .fg(Color::White)
        .add_modifier(Modifier::BOLD)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::presentation::journal::{Message, Voice};
    use crate::presentation::transcript;

    #[test]
    fn thought_tone_preserves_inline_emphasis_and_link_decoration() {
        let message = Message {
            voice: Voice::Thought,
            text: "*Inspect* [guide](https://example.test/guide)".into(),
            complete: true,
        };
        let lines = rows(transcript::message(&message, 80));
        assert_eq!(
            lines.iter().map(ToString::to_string).collect::<Vec<_>>(),
            ["· Inspect guide"]
        );
        let spans: Vec<_> = lines.iter().flat_map(|line| &line.spans).collect();
        assert!(spans.iter().all(|span| {
            span.style.fg == Some(Color::White)
                && span.style.add_modifier.contains(Modifier::BOLD)
                && !span.style.add_modifier.contains(Modifier::DIM)
        }));
        assert!(
            spans.iter().any(|span| span.content == "Inspect"
                && span.style.add_modifier.contains(Modifier::ITALIC))
        );
        assert!(spans.iter().any(|span| span.content == "guide"
            && span.style.add_modifier.contains(Modifier::UNDERLINED)));
    }

    #[test]
    fn formatted_heading_and_code_keep_their_terminal_styles() {
        let lines = rows(transcript::markdown::render(
            "# Title\n\n```sh\necho '**raw**'\n```",
            40,
        ));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content == "Title"
                    // Inline text supplies Reset over the heading's default tone.
                    && span.style.fg == Some(Color::Reset)
                && span
                    .style
                    .add_modifier
                    .contains(Modifier::BOLD | Modifier::UNDERLINED)
        }));
        assert!(lines.iter().flat_map(|line| &line.spans).any(|span| {
            span.content.contains("echo '**raw**'")
                && span.style.fg == Some(Color::DarkGray)
                && span.style.add_modifier.is_empty()
        }));
    }
}
