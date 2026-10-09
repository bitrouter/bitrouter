//! Shared grapheme-aware layout and cursor positioning for native and ACP drafts.
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

#[derive(Clone)]
pub struct ComposerLayout {
    pub rows: Vec<Line<'static>>,
    pub cursor_row: usize,
    pub cursor_column: u16,
}

#[derive(Clone, Copy)]
struct ComposerFormat {
    cursor: usize,
    content_width: u16,
    rail: &'static str,
    rail_style: Style,
}

pub fn layout(
    text: &str,
    cursor: usize,
    content_width: u16,
    rail: &'static str,
    rail_style: Style,
) -> ComposerLayout {
    let mut rows = Vec::new();
    let mut cursor_position = None;
    let format = ComposerFormat {
        cursor,
        content_width,
        rail,
        rail_style,
    };
    let mut start = 0_usize;
    let bytes = text.as_bytes();
    let mut index = 0_usize;

    while index < bytes.len() {
        let newline = matches!(bytes[index], b'\n' | b'\r');
        if !newline {
            index = index.saturating_add(1);
            continue;
        }
        append_composer_line(
            &mut rows,
            &mut cursor_position,
            &text[start..index],
            start,
            &format,
        );
        let mut next = index.saturating_add(1);
        if bytes[index] == b'\r' && bytes.get(next) == Some(&b'\n') {
            next = next.saturating_add(1);
        }
        if cursor > index && cursor < next {
            cursor_position = rows.last().map(|line| {
                (
                    rows.len().saturating_sub(1),
                    u16::try_from(line_width(line)).unwrap_or(u16::MAX),
                )
            });
        }
        start = next;
        index = next;
    }
    append_composer_line(
        &mut rows,
        &mut cursor_position,
        &text[start..],
        start,
        &format,
    );
    let (cursor_row, cursor_column) = cursor_position.unwrap_or_else(|| {
        let row = rows.len().saturating_sub(1);
        let column = rows
            .last()
            .map(|line| u16::try_from(line_width(line)).unwrap_or(u16::MAX))
            .unwrap_or(2);
        (row, column)
    });
    ComposerLayout {
        rows,
        cursor_row,
        cursor_column,
    }
}

fn append_composer_line(
    rows: &mut Vec<Line<'static>>,
    cursor_position: &mut Option<(usize, u16)>,
    text: &str,
    start: usize,
    format: &ComposerFormat,
) {
    let mut content = String::new();
    let mut cells = 0_u16;
    let width = format.content_width.max(1);
    let mut row_start = start;

    for (offset, grapheme) in text.grapheme_indices(true) {
        let position = start.saturating_add(offset);
        if cursor_position.is_none() && format.cursor == position {
            *cursor_position = Some((
                rows.len(),
                u16::try_from(2_usize.saturating_add(usize::from(cells))).unwrap_or(u16::MAX),
            ));
        }
        let rendered = sanitize(grapheme);
        let grapheme_cells = u16::try_from(rendered.width()).unwrap_or(u16::MAX);
        if !content.is_empty() && cells.saturating_add(grapheme_cells) > width {
            rows.push(composer_row(
                std::mem::take(&mut content),
                format.rail,
                format.rail_style,
            ));
            row_start = position;
            cells = 0;
        }
        if cursor_position.is_none() && format.cursor == row_start {
            *cursor_position = Some((rows.len(), 2));
        }
        content.push_str(&rendered);
        cells = cells.saturating_add(grapheme_cells);
    }
    let end = start.saturating_add(text.len());
    if cursor_position.is_none() && format.cursor == end {
        *cursor_position = Some((
            rows.len(),
            u16::try_from(2_usize.saturating_add(usize::from(cells))).unwrap_or(u16::MAX),
        ));
    }
    rows.push(composer_row(content, format.rail, format.rail_style));
}

fn composer_row(content: String, rail: &'static str, rail_style: Style) -> Line<'static> {
    Line::from(vec![
        Span::styled(format!("{rail} "), rail_style),
        Span::raw(content),
    ])
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|span| span.content.width()).sum()
}

fn sanitize(text: &str) -> String {
    text.replace('\u{1b}', "␛")
        .replace('\r', "")
        .replace('\t', "    ")
}
