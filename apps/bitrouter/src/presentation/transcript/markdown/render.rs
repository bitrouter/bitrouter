// Adapted from ratatui-markdown 0.3.6, src/markdown/render.rs, by langyo.
// https://github.com/celestia-island/ratatui-markdown
// Modified for BitRouter's backend-independent transcript types and fixed style.
// Distributed under Apache-2.0; see LICENSE and NOTICE in this directory.

use super::{
    Format, Renderer, Row, Run, Tone, code_block,
    inline::parse_inline_formatting,
    types::{MarkdownBlock, TextToken},
};

const HLINE: &str = "─";
const VLINE: &str = "│";
const CORNER_TL: &str = "┌";
const CORNER_TR: &str = "┐";
const CORNER_BL: &str = "└";
const CORNER_BR: &str = "┘";
const TOP_MID: &str = "┬";
const BOTTOM_MID: &str = "┴";
const MID_LEFT: &str = "├";
const MID_RIGHT: &str = "┤";
const CROSS_MID: &str = "┼";

impl Renderer {
    pub(super) fn render(&self, blocks: &[MarkdownBlock]) -> Vec<Row> {
        let mut lines = Vec::new();
        for block in blocks {
            self.render_block(block, &mut lines);
        }
        lines
    }

    fn render_block(&self, block: &MarkdownBlock, lines: &mut Vec<Row>) {
        match block {
            MarkdownBlock::Heading1(text)
            | MarkdownBlock::Heading2(text)
            | MarkdownBlock::Heading3(text) => {
                let style = match block {
                    MarkdownBlock::Heading1(_) => {
                        Format::default().tone(Tone::Action).bold().underlined()
                    }
                    MarkdownBlock::Heading3(_) => Format::default().tone(Tone::Thought).bold(),
                    _ => Format::default().tone(Tone::Text).bold(),
                };
                let parsed = parse_inline_formatting(text);
                if parsed.is_empty() {
                    lines.push(Row::from(Run::annotated(text.replace('\t', "    "), style)));
                } else {
                    let styled: Vec<Run> = parsed
                        .into_iter()
                        .map(|mut run| {
                            // Upstream patches inline foreground over heading foreground.
                            run.format = style.patch(run.format);
                            run
                        })
                        .collect();
                    lines.push(Row::from(styled));
                }
            }
            MarkdownBlock::Paragraph(paragraph_lines) => {
                for line in paragraph_lines {
                    lines.extend(self.wrap_text_with_inline_formatting(line));
                }
            }
            MarkdownBlock::CodeBlock(code) => {
                // The renderer width originates from render's normalized u16.
                lines.extend(code_block(code, self.max_width as u16));
            }
            MarkdownBlock::ListItem(text, indent) => {
                let indent_str = "  ".repeat(usize::from(*indent));
                lines.extend(
                    self.wrap_text_with_inline_formatting(&format!("{indent_str}•  {text}")),
                );
            }
            MarkdownBlock::TaskItem {
                text,
                indent,
                checked,
            } => {
                let indent_str = "  ".repeat(usize::from(*indent));
                let checkbox = if *checked { "☑ " } else { "☐ " };
                lines.extend(
                    self.wrap_text_with_inline_formatting(&format!("{indent_str}{checkbox}{text}")),
                );
            }
            MarkdownBlock::Blockquote { level, children } => {
                let prefix_str = "│ ".repeat(usize::from(*level));
                let prefix_style = Format::default().tone(Tone::Muted);
                for mut line in self.render(children) {
                    line.runs
                        .insert(0, Run::annotated(prefix_str.clone(), prefix_style));
                    for run in line.runs.iter_mut().skip(1) {
                        run.format = run.format.tone(Tone::Muted).italic();
                    }
                    lines.push(line);
                }
            }
            MarkdownBlock::HorizontalRule => {
                lines.push(Row::from(Run::annotated(
                    HLINE.repeat(self.max_width.min(80)),
                    Format::default().tone(Tone::Muted),
                )));
            }
            MarkdownBlock::BlankLine => lines.push(Row::from("")),
            MarkdownBlock::Table { headers, rows } => {
                lines.extend(self.render_table(headers, rows))
            }
            MarkdownBlock::ImageLabel(label) => {
                lines.push(Row::from(Run::annotated(
                    format!("[image: {}]", label.replace('\t', "    ")),
                    Format::default().tone(Tone::Code).italic(),
                )));
            }
        }
    }

    fn render_table(&self, headers: &[String], rows: &[Vec<String>]) -> Vec<Row> {
        let mut lines = Vec::new();
        let col_count = headers
            .len()
            .max(rows.iter().map(|r| r.len()).fold(0, usize::max));
        let padding_per_cell: usize = 2;
        let border_overhead = col_count + 1;
        let total_padding = col_count * padding_per_cell;
        let available = if self.max_width > border_overhead + total_padding {
            self.max_width
                .saturating_sub(border_overhead + total_padding)
        } else {
            80_usize.saturating_sub(border_overhead + total_padding)
        };

        fn longest_token_width(text: &str) -> usize {
            Renderer::tokenize(text)
                .into_iter()
                .filter_map(|t| match t {
                    TextToken::Word(w) => Some(Renderer::string_width(&w)),
                    _ => None,
                })
                .fold(0, usize::max)
        }

        let header_widths: Vec<usize> = (0..col_count)
            .map(|c| headers.get(c).map_or(0, |h| Self::string_width(h)))
            .collect();

        let min_widths: Vec<usize> = (0..col_count)
            .map(|c| {
                let h_longest = headers.get(c).map_or(0, |h| longest_token_width(h));
                let d_longest = rows
                    .iter()
                    .filter_map(|r| r.get(c))
                    .map(|cell| longest_token_width(cell))
                    .fold(0, usize::max);
                h_longest.max(d_longest).max(3)
            })
            .collect();

        let natural_widths: Vec<usize> = (0..col_count)
            .map(|c| {
                let hw = header_widths[c];
                let rw = rows
                    .iter()
                    .filter_map(|r| r.get(c))
                    .map(|cell| Self::string_width(cell))
                    .fold(0, usize::max);
                hw.max(rw)
            })
            .collect();

        let target_lines: usize = 3;
        let ideal_widths: Vec<usize> = (0..col_count)
            .map(|c| {
                let natural = natural_widths[c];
                if natural == 0 {
                    return min_widths[c];
                }
                let wrapped = natural.div_ceil(target_lines);
                wrapped.max(min_widths[c])
            })
            .collect();

        let total_ideal: usize = ideal_widths.iter().sum();
        let mut col_widths: Vec<usize> = if total_ideal <= available {
            (0..col_count)
                .map(|c| ideal_widths[c].max(min_widths[c]))
                .collect()
        } else {
            (0..col_count)
                .map(|c| {
                    let proportional =
                        (available as u64 * ideal_widths[c] as u64 / total_ideal as u64) as usize;
                    proportional.max(min_widths[c])
                })
                .collect()
        };

        let mut total_allocated: usize = col_widths.iter().sum();
        if total_allocated > available {
            let deficit = total_allocated - available;
            let mut remaining = deficit;
            let mut sorted: Vec<usize> = (0..col_count).collect();
            sorted.sort_by_key(|&i| std::cmp::Reverse(col_widths[i] - min_widths[i]));
            for idx in sorted {
                if remaining == 0 {
                    break;
                }
                let shrinkable = col_widths[idx] - min_widths[idx];
                let take = shrinkable.min(remaining);
                col_widths[idx] -= take;
                remaining -= take;
            }
            total_allocated = col_widths.iter().sum();
        }
        if total_allocated < available {
            let mut surplus = available - total_allocated;
            let total_natural: usize = natural_widths.iter().sum::<usize>().max(1);
            while surplus > 0 {
                let mut gave_this_round = false;
                for idx in 0..col_count {
                    if surplus == 0 {
                        break;
                    }
                    let share = ((surplus * natural_widths[idx]) / total_natural).max(1);
                    let take = share.min(surplus);
                    col_widths[idx] += take;
                    surplus -= take;
                    gave_this_round = true;
                }
                if !gave_this_round {
                    break;
                }
            }
        }

        let border_style = Format::default().tone(Tone::Muted);

        lines.push(Row::from(Run::annotated(
            Self::build_table_hline(&col_widths, CORNER_TL, TOP_MID, CORNER_TR),
            border_style,
        )));
        let header_line_spans: Vec<Vec<Vec<Run>>> = (0..col_count)
            .map(|c| {
                let text = headers.get(c).map_or("", String::as_str);
                let inner = col_widths[c].saturating_sub(2);
                let base_style = Format::default().tone(Tone::Text).bold();
                let parsed = parse_inline_formatting(text);
                let spans = if parsed.is_empty() {
                    vec![Run::annotated(text.to_string(), base_style)]
                } else {
                    parsed
                        .into_iter()
                        .map(|mut s| {
                            s.format = base_style.patch(s.format);
                            s
                        })
                        .collect()
                };
                Self::wrap_styled_spans_to_width(spans, inner)
            })
            .collect();
        let header_height = header_line_spans
            .iter()
            .map(|l| l.len().max(1))
            .fold(1, usize::max);
        for line_idx in 0..header_height {
            let line_cells: Vec<Vec<Run>> = (0..col_count)
                .map(|c| {
                    if line_idx < header_line_spans[c].len() {
                        header_line_spans[c][line_idx].clone()
                    } else {
                        vec![]
                    }
                })
                .collect();
            lines.push(Self::build_table_row_from_spans(
                &col_widths,
                &line_cells,
                true,
            ));
        }
        lines.push(Row::from(Run::annotated(
            Self::build_table_hline(&col_widths, MID_LEFT, CROSS_MID, MID_RIGHT),
            border_style,
        )));

        for row in rows {
            let cell_line_spans: Vec<Vec<Vec<Run>>> = (0..col_count)
                .map(|c| {
                    let text = row.get(c).map_or("", String::as_str);
                    let inner = col_widths[c].saturating_sub(2);
                    let base_style = Format::default().tone(Tone::Text);
                    let parsed = parse_inline_formatting(text);
                    let spans = if parsed.is_empty() {
                        vec![Run::annotated(text.to_string(), base_style)]
                    } else {
                        parsed
                    };
                    Self::wrap_styled_spans_to_width(spans, inner)
                })
                .collect();
            let row_height = cell_line_spans
                .iter()
                .map(|l| l.len().max(1))
                .fold(1, usize::max);
            for line_idx in 0..row_height {
                let line_cells: Vec<Vec<Run>> = (0..col_count)
                    .map(|c| {
                        if line_idx < cell_line_spans[c].len() {
                            cell_line_spans[c][line_idx].clone()
                        } else {
                            vec![]
                        }
                    })
                    .collect();
                lines.push(Self::build_table_row_from_spans(
                    &col_widths,
                    &line_cells,
                    false,
                ));
            }
            lines.push(Row::from(Run::annotated(
                Self::build_table_hline(&col_widths, MID_LEFT, CROSS_MID, MID_RIGHT),
                border_style,
            )));
        }

        let last_hline = Self::build_table_hline(&col_widths, CORNER_BL, BOTTOM_MID, CORNER_BR);
        if let Some(last) = lines.last_mut() {
            *last = Row::from(Run::annotated(last_hline, border_style));
        }

        lines
    }

    pub(super) fn build_table_hline(
        col_widths: &[usize],
        left: &str,
        mid: &str,
        right: &str,
    ) -> String {
        let mut parts = vec![left.to_string()];
        for width in col_widths.iter() {
            parts.push(HLINE.repeat(*width));
            parts.push(mid.to_string());
        }
        parts.pop();
        parts.push(right.to_string());
        parts.join("")
    }

    pub(super) fn wrap_styled_spans_to_width(spans: Vec<Run>, max_w: usize) -> Vec<Vec<Run>> {
        if max_w == 0 || spans.is_empty() {
            return if spans.is_empty() {
                vec![vec![]]
            } else {
                vec![spans]
            };
        }
        let mut lines: Vec<Vec<Run>> = Vec::new();
        let mut current_line: Vec<Run> = Vec::new();
        let mut current_width: usize = 0;
        let mut pending_space = false;

        for span in spans {
            let style = span.format;
            let text = span.content.to_string();
            let tokens = Self::tokenize(&text);

            for token in tokens {
                match token {
                    TextToken::Newline => {
                        lines.push(std::mem::take(&mut current_line));
                        current_width = 0;
                        pending_space = false;
                    }
                    TextToken::Space => {
                        pending_space = true;
                    }
                    TextToken::Word(word) => {
                        let word_w = Self::string_width(&word);
                        let space_w: usize = if pending_space && current_width > 0 {
                            1
                        } else {
                            0
                        };
                        let needs_wrap = if current_width == 0 {
                            false
                        } else if space_w > 0 && current_width + space_w >= max_w {
                            true
                        } else {
                            current_width + space_w + word_w > max_w
                        };
                        if needs_wrap && !current_line.is_empty() {
                            lines.push(std::mem::take(&mut current_line));
                            current_width = 0;
                            pending_space = false;
                        }
                        let final_space = if pending_space && current_width > 0 {
                            pending_space = false;
                            1
                        } else {
                            0
                        };
                        if final_space > 0 {
                            current_line.push(Run::annotated(" ".to_string(), style));
                            current_width += final_space;
                        }
                        if current_width == 0 && word_w > max_w && max_w > 0 {
                            let mut chars: Vec<char> = word.chars().collect();
                            let mut char_w = 0;
                            let mut chunk = String::new();
                            for ch in chars.drain(..) {
                                let cw = Self::string_width(&ch.to_string());
                                if char_w + cw > max_w && !chunk.is_empty() {
                                    current_line.push(Run::annotated(chunk, style));
                                    lines.push(std::mem::take(&mut current_line));
                                    chunk = String::new();
                                    char_w = 0;
                                }
                                chunk.push(ch);
                                char_w += cw;
                            }
                            if !chunk.is_empty() {
                                current_line.push(Run::annotated(chunk, style));
                                current_width += char_w;
                            }
                        } else {
                            current_line.push(Run::annotated(word, style));
                            current_width += word_w;
                        }
                    }
                }
            }
        }
        if !current_line.is_empty() || lines.is_empty() {
            lines.push(current_line);
        }
        lines
    }

    pub(super) fn build_table_row_from_spans(
        col_widths: &[usize],
        cell_spans: &[Vec<Run>],
        is_header: bool,
    ) -> Row {
        let base_style = if is_header {
            Format::default().tone(Tone::Text).bold()
        } else {
            Format::default().tone(Tone::Text)
        };

        let mut spans = Vec::new();
        spans.push(Run::annotated(
            VLINE.to_string(),
            Format::default().tone(Tone::Muted),
        ));
        for (i, width) in col_widths.iter().enumerate() {
            let cell_spans_ref = cell_spans.get(i).map_or(&[][..], Vec::as_slice);
            let total_cell_w: usize = cell_spans_ref
                .iter()
                .map(|s| Self::string_width(&s.content))
                .sum();
            let inner_w = width.saturating_sub(2);

            spans.push(Run::annotated("  ".to_string(), base_style));
            spans.extend_from_slice(cell_spans_ref);

            let padding = inner_w.saturating_sub(total_cell_w);
            if padding > 0 {
                spans.push(Run::annotated(" ".repeat(padding), base_style));
            }
            spans.push(Run::annotated(
                VLINE.to_string(),
                Format::default().tone(Tone::Muted),
            ));
        }
        Row::from(spans)
    }
}
