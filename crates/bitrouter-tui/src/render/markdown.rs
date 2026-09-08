//! Restrained Markdown presentation for the interactive conversation.
//!
//! The journal retains source text for copy/search. Presentation preserves
//! source characters too, so incomplete streaming delimiters remain legible.

use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};

/// Style paragraphs, headings, lists, fenced code, inline code, and links.
/// Unknown syntax remains ordinary text rather than being discarded.
pub fn render(text: &str) -> Vec<Line<'static>> {
    let mut fence: Option<(char, usize)> = None;
    text.split('\n')
        .map(|line| {
            let trimmed = line.trim_start();
            let delimiter = fence_delimiter(trimmed);
            if let Some((marker, length)) = delimiter {
                match fence {
                    None => fence = Some((marker, length)),
                    Some((open, size))
                        if open == marker
                            && length >= size
                            && trimmed[length..].trim().is_empty() =>
                    {
                        fence = None
                    }
                    _ => {}
                }
                return Line::styled(
                    line.to_string(),
                    Style::default().add_modifier(Modifier::DIM),
                );
            }
            if fence.is_some() {
                // Code uses the terminal's normal foreground so it remains
                // readable with both light and dark terminal themes.
                return Line::raw(line.to_string());
            }
            let heading = trimmed.chars().take_while(|c| *c == '#').count();
            if (1..=6).contains(&heading)
                && trimmed.get(heading..).is_some_and(|s| s.starts_with(' '))
            {
                return Line::styled(
                    line.to_string(),
                    Style::default().add_modifier(Modifier::BOLD),
                );
            }
            Line::from(inline(line))
        })
        .collect()
}

fn fence_delimiter(line: &str) -> Option<(char, usize)> {
    let first = line.chars().next()?;
    if !matches!(first, '`' | '~') {
        return None;
    }
    let length = line.chars().take_while(|c| *c == first).count();
    (length >= 3).then_some((first, length))
}

fn inline(line: &str) -> Vec<Span<'static>> {
    let mut spans = Vec::new();
    let mut rest = line;
    while !rest.is_empty() {
        let next = rest
            .char_indices()
            .find(|(_, c)| matches!(c, '`' | '*' | '['));
        let Some((index, marker)) = next else {
            spans.push(Span::raw(rest.to_string()));
            break;
        };
        if index > 0 {
            spans.push(Span::raw(rest[..index].to_string()));
            rest = &rest[index..];
        }
        let styled = match marker {
            '`' => delimited(rest, "`")
                .map(|length| (length, Style::default().add_modifier(Modifier::BOLD))),
            '*' if rest.starts_with("**") => delimited(rest, "**")
                .map(|length| (length, Style::default().add_modifier(Modifier::BOLD))),
            '[' => rest
                .find("](")
                .and_then(|middle| rest[middle + 2..].find(')').map(|end| middle + 3 + end))
                .map(|length| (length, Style::default().add_modifier(Modifier::UNDERLINED))),
            _ => None,
        };
        if let Some((length, style)) = styled {
            spans.push(Span::styled(rest[..length].to_string(), style));
            rest = &rest[length..];
        } else {
            let length = marker.len_utf8();
            spans.push(Span::raw(rest[..length].to_string()));
            rest = &rest[length..];
        }
    }
    if spans.is_empty() {
        spans.push(Span::raw(""));
    }
    spans
}

fn delimited(text: &str, delimiter: &str) -> Option<usize> {
    text[delimiter.len()..]
        .find(delimiter)
        .map(|index| index + delimiter.len() * 2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| {
                line.spans
                    .iter()
                    .map(|span| span.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn code_and_streaming_delimiters_keep_source_bytes() {
        let source =
            "# Heading\n\n- item\n```rust\n  let 中文 = `literal`;  \n\n```\n**unfinished\n";
        let lines = render(source);
        assert_eq!(plain(&lines), source);
        assert!(lines[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[4].style, Style::default());
    }

    #[test]
    fn links_and_inline_code_remain_complete_and_distinct() {
        let source = "See [docs](https://example.invalid/path) and `code` plus **bold**.";
        let lines = render(source);
        assert_eq!(plain(&lines), source);
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::UNDERLINED))
        );
        assert!(
            lines[0]
                .spans
                .iter()
                .any(|span| span.style.add_modifier.contains(Modifier::BOLD))
        );
    }
}
