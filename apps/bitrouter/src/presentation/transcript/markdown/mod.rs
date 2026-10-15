//! Shared formatted Markdown for transcript consumers.
//!
//! Parser, inline formatting and layout are adapted from ratatui-markdown 0.3.6:
//! <https://github.com/celestia-island/ratatui-markdown>
//! See NOTICE and LICENSE for the pinned source and upstream attribution.
//! BitRouter's table fallback and literal code frames are applied here once.

mod inline;
mod parser;
mod render;
#[cfg(test)]
mod tests;
mod text;
mod types;
#[cfg(test)]
mod wrapping_tests;

use super::text::{Format, Row, Run, Tone};
use types::MarkdownBlock;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

struct Renderer {
    max_width: usize,
}

/// Render accumulated text, including incomplete streamed Markdown.
pub(crate) fn render(text: &str, width: u16) -> Vec<Row> {
    let width = usize::from(width.max(1));
    let renderer = Renderer { max_width: width };
    let mut blocks = Vec::new();
    for block in Renderer::parse(text) {
        if let MarkdownBlock::Table { headers, rows } = &block
            && !rows.is_empty()
            && renderer
                .render(std::slice::from_ref(&block))
                .iter()
                .any(|line| line.width() > width)
        {
            // Preserve every value when the upstream table sizing exceeds the
            // viewport, including long link destinations in column sizing.
            for row in rows {
                for (index, value) in row.iter().enumerate() {
                    let label = headers.get(index).map_or("", String::as_str);
                    blocks.push(MarkdownBlock::Paragraph(vec![format!(
                        "**{label}:** {value}"
                    )]));
                }
                blocks.push(MarkdownBlock::BlankLine);
            }
        } else {
            blocks.push(block);
        }
    }
    let mut lines = renderer.render(&blocks);
    if lines.is_empty() {
        lines.push(Row::default());
    }
    lines
}

/// A literal, subdued code frame. Wrap graphemes rather than words so shell
/// whitespace, quoting and indentation survive; Markdown never parses code.
pub(crate) fn code_block(code: &str, width: u16) -> Vec<Row> {
    let width = usize::from(width.max(1));
    let framed = width >= 6;
    let inner = if framed { width - 4 } else { width };
    let style = Format::default().tone(Tone::Muted);
    let mut lines = Vec::new();
    if framed {
        lines.push(Row::annotated(
            format!("┌{}┐", "─".repeat(width - 2)),
            style,
        ));
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
        lines.push(Row::annotated(
            format!("└{}┘", "─".repeat(width - 2)),
            style,
        ));
    }
    lines
}

fn push_code_row(
    lines: &mut Vec<Row>,
    row: &str,
    cells: usize,
    inner: usize,
    framed: bool,
    style: Format,
) {
    let text = if framed {
        format!("│ {row}{} │", " ".repeat(inner.saturating_sub(cells)))
    } else {
        row.to_string()
    };
    lines.push(Row::from(Run::annotated(text, style)));
}
