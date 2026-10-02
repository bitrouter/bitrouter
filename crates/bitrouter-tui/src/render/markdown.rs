//! Markdown parsing and layout belong to ratatui-markdown. This adapter supplies
//! BitRouter's code-frame style and a readable fallback for oversized tables.

use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui_markdown::markdown::{MarkdownBlock, MarkdownRenderer, RenderHooks};
use ratatui_markdown::theme::ThemeConfig;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

/// Render the accumulated message, including incomplete streamed Markdown.
pub fn render(text: &str, width: u16) -> Vec<Line<'static>> {
    let width = width.max(1);
    let renderer =
        MarkdownRenderer::new(usize::from(width)).with_render_hooks(Box::new(CodeFrame { width }));
    let theme = ThemeConfig::default()
        .with_text_color(Color::Reset)
        .with_primary_color(Color::Cyan)
        .with_secondary_color(Color::White)
        .with_accent_yellow(Color::Gray);
    let mut blocks = Vec::new();
    for block in renderer.parse(text) {
        if let MarkdownBlock::Table { headers, rows } = &block
            && !rows.is_empty()
            && renderer
                .render(std::slice::from_ref(&block), &theme)
                .iter()
                .any(|line| line.width() > usize::from(width))
        {
            // The library preserves long tokens when sizing table columns. If
            // that exceeds the viewport, show each record as labeled fields;
            // keep every value instead of clipping it or breaking the grid.
            for row in rows {
                for (index, value) in row.iter().enumerate() {
                    let label = headers.get(index).map(String::as_str).unwrap_or("");
                    let field = MarkdownBlock::Paragraph(vec![format!("**{label}:** {value}")]);
                    blocks.push(field);
                }
                blocks.push(MarkdownBlock::BlankLine);
            }
        } else {
            blocks.push(block);
        }
    }
    let mut lines = renderer.render(&blocks, &theme);
    if lines.is_empty() {
        lines.push(Line::default());
    }
    lines
}

struct CodeFrame {
    width: u16,
}

impl RenderHooks for CodeFrame {
    fn render_code_block(&self, _lang: &str, content: &str) -> Option<Vec<Line<'static>>> {
        Some(code_block(content, self.width))
    }
}

/// A literal, subdued code frame. Wrap graphemes rather than words so shell
/// whitespace, quoting and indentation survive; Markdown never parses code.
pub fn code_block(code: &str, width: u16) -> Vec<Line<'static>> {
    let width = usize::from(width.max(1));
    let framed = width >= 6;
    let inner = if framed { width - 4 } else { width };
    let style = Style::default().fg(Color::DarkGray);
    let mut lines = Vec::new();
    if framed {
        lines.push(Line::styled(format!("┌{}┐", "─".repeat(width - 2)), style));
    }
    for line in code.split('\n') {
        let expanded = line.replace('\t', "    ");
        let mut row = String::new();
        let mut cells = 0;
        for grapheme in expanded.graphemes(true) {
            let size = grapheme.width();
            if cells + size > inner && !row.is_empty() {
                push_code_row(&mut lines, &row, cells, inner, framed, style);
                row.clear();
                cells = 0;
            }
            row.push_str(grapheme);
            cells += size;
        }
        push_code_row(&mut lines, &row, cells, inner, framed, style);
    }
    if framed {
        lines.push(Line::styled(format!("└{}┘", "─".repeat(width - 2)), style));
    }
    lines
}

fn push_code_row(
    lines: &mut Vec<Line<'static>>,
    row: &str,
    cells: usize,
    inner: usize,
    framed: bool,
    style: Style,
) {
    let text = if framed {
        format!("│ {row}{} │", " ".repeat(inner.saturating_sub(cells)))
    } else {
        row.to_string()
    };
    lines.push(Line::from(Span::styled(text, style)));
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::style::Modifier;

    fn text(lines: &[Line<'_>]) -> String {
        lines
            .iter()
            .map(|line| line.to_string())
            .collect::<Vec<_>>()
            .join("\n")
    }

    #[test]
    fn markdown_formats_prose_tables_links_and_literal_code() {
        let source = "## Workspace\n\n**Router** and *cloud*. Use `bitrouter`.\n\n- First\n- Second\n\n| Repo | Role |\n|---|---|\n| [bitrouter](/tmp/README.md) | Router |\n\n```sh\necho '**literal**'\n```";
        let lines = render(source, 80);
        let output = text(&lines);
        assert!(output.starts_with("Workspace\n"), "{output}");
        assert!(
            !output.contains("**Router**") && !output.contains("*cloud*"),
            "{output}"
        );
        assert!(
            !output.contains("[bitrouter]") && !output.contains("/tmp/README.md"),
            "{output}"
        );
        assert!(
            output.contains("┬") && output.contains("Repo") && output.contains("Role"),
            "{output}"
        );
        assert!(output.contains("echo '**literal**'"), "{output}");
        assert!(!output.contains("```"), "{output}");
        assert!(lines.iter().flat_map(|line| &line.spans).any(
            |span| span.content == "Router" && span.style.add_modifier.contains(Modifier::BOLD)
        ));
    }

    #[test]
    fn narrow_tables_retain_each_labeled_value_and_hide_link_syntax() {
        let source = "| Repository | Role |\n|---|---|\n| [bitrouter](/Users/example/a/very/long/workspace/path/README.md) | Routes requests safely |\n| cloud | Hosted platform |";
        let lines = render(source, 24);
        let output = text(&lines);
        assert!(lines.iter().all(|line| line.width() <= 24), "{output}");
        assert!(output.contains("Repository: bitrouter"), "{output}");
        assert!(output.contains("Role: Routes requests"), "{output}");
        assert!(
            output.contains("safely") && output.contains("Hosted platform"),
            "{output}"
        );
        assert!(
            !output.contains("/Users/") && !output.contains("[bitrouter]"),
            "{output}"
        );
    }

    #[test]
    fn streaming_reparses_completed_formatting_and_unclosed_fences() {
        let partial = render("**Inspecting", 40);
        assert!(text(&partial).contains("Inspecting"));
        assert_eq!(
            text(&render("**Inspecting files**", 40)),
            "Inspecting files"
        );
        let code = text(&render("```sh\nprintf '**raw**'", 40));
        assert!(code.contains("printf '**raw**'"), "{code}");
        assert!(code.contains('┌') && code.contains('└'), "{code}");
    }

    #[test]
    fn command_frames_preserve_wrapped_graphemes_and_indentation() {
        let code = "  echo '你好👩‍💻**raw**'";
        let lines = code_block(code, 16);
        assert!(
            lines.iter().all(|line| line.width() == 16),
            "{}",
            text(&lines)
        );
        let restored: String = lines
            .iter()
            .skip(1)
            .take(lines.len() - 2)
            .map(|line| {
                line.to_string()
                    .trim_start_matches("│ ")
                    .trim_end_matches(" │")
                    .trim_end()
                    .to_string()
            })
            .collect();
        assert_eq!(restored, code);
        assert!(
            lines
                .iter()
                .flat_map(|line| &line.spans)
                .all(|span| !span.style.add_modifier.contains(Modifier::BOLD))
        );
    }
}

/// Preserve source characters for the Code journal's reading anchors.
/// Style paragraphs, headings, lists, fenced code, inline code, and links.
/// Unknown syntax remains ordinary text rather than being discarded.
pub fn source_lines(text: &str) -> Vec<Line<'static>> {
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
mod source_tests {
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
        let lines = source_lines(source);
        assert_eq!(plain(&lines), source);
        assert!(lines[0].style.add_modifier.contains(Modifier::BOLD));
        assert_eq!(lines[4].style, Style::default());
    }

    #[test]
    fn links_and_inline_code_remain_complete_and_distinct() {
        let source = "See [docs](https://example.invalid/path) and `code` plus **bold**.";
        let lines = source_lines(source);
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
