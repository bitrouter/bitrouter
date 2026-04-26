use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{
    Block, Borders, Clear, List, ListItem, Paragraph, Scrollbar, ScrollbarOrientation,
    ScrollbarState,
};

use crate::app::{AppState, InputMode};
use crate::model::{RenderContext, Renderable};

const PROMPT_PREFIX: &str = "› ";

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let width = area.width;
    let ctx = build_render_context(state);

    let has_scrollback = state
        .active_scrollback()
        .is_some_and(|sb| !sb.entries.is_empty());

    // Empty-state hints (rendered directly, no caching needed).
    let mut empty_lines: Vec<Line> = Vec::new();
    if !has_scrollback && state.input.is_empty() && state.session_store.active.is_empty() {
        let half = area.height / 2;
        for _ in 0..half {
            empty_lines.push(Line::raw(""));
        }
        empty_lines.push(Line::from(Span::styled(
            "  Type a message to start — use @agent to address a specific agent",
            Style::default().fg(Color::DarkGray),
        )));
        empty_lines.push(Line::from(Span::styled(
            "  Press Alt+A to connect an agent",
            Style::default().fg(Color::DarkGray),
        )));
    } else if !has_scrollback && state.input.is_empty() {
        let half = area.height / 2;
        for _ in 0..half {
            empty_lines.push(Line::raw(""));
        }
        let agent_name = state.active_agent_name().unwrap_or("agent");
        empty_lines.push(Line::from(Span::styled(
            format!("  Connected to {agent_name} — type a message to start"),
            Style::default().fg(Color::DarkGray),
        )));
    }

    // Fill line-count cache for all entries.
    if let Some(sb) = state.active_scrollback_mut() {
        // Invalidate all on width change.
        if sb.cached_width != width {
            sb.invalidate_all();
            sb.cached_width = width;
        }

        // Fill uncached slots.
        for i in 0..sb.entries.len() {
            if sb.line_counts.get(i).copied().flatten().is_none() {
                let count = sb.entries[i]
                    .kind
                    .render_lines(width, sb.entries[i].collapsed, &ctx)
                    .len();
                if i < sb.line_counts.len() {
                    sb.line_counts[i] = Some(count);
                }
            }
        }

        let total_entry_lines = sb.rebuild_offsets();

        // Prompt lines.
        let prompt_lines = prompt_line_count(state);
        let total = empty_lines.len() + total_entry_lines + prompt_lines;
        let visible = area.height as usize;

        // Update total_rendered_lines and clamp scroll.
        if let Some(sb) = state.active_scrollback_mut() {
            sb.total_rendered_lines = total;
            let max_scroll = total.saturating_sub(visible);
            if sb.follow {
                sb.scroll_offset = max_scroll;
            } else {
                sb.scroll_offset = sb.scroll_offset.min(max_scroll);
            }
        }
    }

    // Now build the visible lines using viewport math.
    let visible = area.height as usize;
    let scroll_offset = state
        .active_scrollback()
        .map(|sb| sb.scroll_offset)
        .unwrap_or(0);

    let mut lines: Vec<Line> = Vec::new();

    // Figure out what's visible: empty_lines first, then entries, then prompt.
    let empty_count = empty_lines.len();
    let entry_total = state
        .active_scrollback()
        .map(|sb| sb.line_offsets.last().copied().unwrap_or(0))
        .unwrap_or(0);

    // Viewport range in the global line space.
    let vp_start = scroll_offset;
    let vp_end = scroll_offset + visible;

    // 1. Empty-state lines (if visible).
    if !empty_lines.is_empty() && vp_start < empty_count {
        let start = vp_start;
        let end = vp_end.min(empty_count);
        lines.extend(empty_lines.into_iter().skip(start).take(end - start));
    }

    // 2. Entry lines (viewport-aware).
    if entry_total > 0 {
        let entry_global_start = empty_count;
        // Only render entries overlapping the viewport.
        if vp_end > entry_global_start && vp_start < entry_global_start + entry_total {
            let local_vp_start = vp_start.saturating_sub(entry_global_start);
            let local_vp_end = (vp_end - entry_global_start).min(entry_total);

            if let Some(sb) = state.active_scrollback() {
                let (first, last) =
                    visible_entry_range(&sb.line_offsets, local_vp_start, local_vp_end);
                let scroll_cursor = sb.scroll_cursor;

                for entry_idx in first..last {
                    if let Some(entry) = sb.entries.get(entry_idx) {
                        let mut entry_lines = entry.kind.render_lines(width, entry.collapsed, &ctx);

                        // Apply scroll cursor highlight.
                        if scroll_cursor == Some(entry_idx) {
                            apply_cursor_highlight(&mut entry_lines);
                        }

                        let entry_start = sb.line_offsets.get(entry_idx).copied().unwrap_or(0);

                        // Clip entry lines to viewport.
                        let skip = local_vp_start.saturating_sub(entry_start);
                        let take = local_vp_end.saturating_sub(entry_start.max(local_vp_start));
                        lines.extend(entry_lines.into_iter().skip(skip).take(take));
                    }
                }
            }
        }
    }

    // 3. Inline prompt lines.
    let prompt_global_start = empty_count + entry_total;
    if vp_end > prompt_global_start {
        let mut prompt_lines_vec: Vec<Line> = Vec::new();
        render_inline_prompt(state, &mut prompt_lines_vec, width);
        let local_start = vp_start.saturating_sub(prompt_global_start);
        let local_end = (vp_end - prompt_global_start).min(prompt_lines_vec.len());
        if local_start < local_end {
            lines.extend(
                prompt_lines_vec
                    .into_iter()
                    .skip(local_start)
                    .take(local_end - local_start),
            );
        }
    }

    let para = Paragraph::new(lines);
    frame.render_widget(para, area);

    // Cursor positioning for inline input. The 3-line prompt header
    // (blank + cwd label + divider) sits before the input rows, so
    // we offset by 3.
    if state.mode == InputMode::Normal {
        let total = empty_count + entry_total + prompt_line_count(state);
        let prompt_start_line = total.saturating_sub(prompt_line_count(state));
        let cursor_line = prompt_start_line + 3 + state.input.cursor.0;
        let cursor_col = if state.input.cursor.0 == 0 {
            PROMPT_PREFIX.len() + state.input.cursor.1
        } else {
            2 + state.input.cursor.1
        };

        let screen_line = cursor_line.saturating_sub(scroll_offset);
        if screen_line < visible {
            frame.set_cursor_position((
                area.x + (cursor_col as u16).min(area.width.saturating_sub(1)),
                area.y + screen_line as u16,
            ));
        }
    }

    // Scrollbar.
    let total = empty_count + entry_total + prompt_line_count(state);
    let max_scroll = total.saturating_sub(visible);
    if max_scroll > 0 {
        let mut scroll_state = ScrollbarState::new(max_scroll).position(scroll_offset);
        let scrollbar = Scrollbar::new(ScrollbarOrientation::VerticalRight);
        let sb_area = Rect::new(
            area.x + area.width.saturating_sub(1),
            area.y,
            1,
            area.height,
        );
        frame.render_stateful_widget(scrollbar, sb_area, &mut scroll_state);
    }

    // Autocomplete popup.
    render_autocomplete(frame, state, area);
}

/// Returns (first_entry_idx, last_entry_idx_exclusive) that overlap the viewport.
fn visible_entry_range(line_offsets: &[usize], vp_start: usize, vp_end: usize) -> (usize, usize) {
    if line_offsets.len() < 2 {
        return (0, 0);
    }
    let entry_count = line_offsets.len() - 1;

    // First visible entry: last entry whose offset <= vp_start.
    let first = line_offsets
        .partition_point(|&o| o <= vp_start)
        .saturating_sub(1)
        .min(entry_count.saturating_sub(1));

    // Last visible entry: first entry whose offset >= vp_end.
    let last = line_offsets
        .partition_point(|&o| o < vp_end)
        .min(entry_count);

    (first, last)
}

/// Apply a visual highlight to the first line of the cursor entry.
fn apply_cursor_highlight(lines: &mut [Line<'static>]) {
    if let Some(first_line) = lines.first_mut() {
        let mut new_spans = vec![Span::styled(
            "▸ ",
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD),
        )];
        new_spans.append(&mut first_line.spans);
        first_line.spans = new_spans;
    }
}

fn render_inline_prompt(state: &AppState, lines: &mut Vec<Line<'static>>, width: u16) {
    // Cwd label + horizontal divider above the input. The cwd is
    // displayed home-relative when possible.
    let cwd = format_cwd(&state.config);
    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!("  {cwd}"),
        Style::default().fg(Color::DarkGray),
    )));
    lines.push(Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(Color::DarkGray),
    )));

    let input_lines = &state.input.lines;
    let focused = state.mode == InputMode::Normal;

    let prompt_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    for (i, line) in input_lines.iter().enumerate() {
        if i == 0 {
            lines.push(Line::from(vec![
                Span::styled(PROMPT_PREFIX.to_string(), prompt_style),
                Span::raw(line.clone()),
            ]));
        } else {
            lines.push(Line::from(vec![Span::raw("  "), Span::raw(line.clone())]));
        }
    }
}

/// Total rendered rows for the input region:
/// 1 blank + 1 cwd label + 1 divider + N input lines.
fn prompt_line_count(state: &AppState) -> usize {
    3 + state.input.lines.len()
}

/// Format the current working directory for display in the input
/// area. Substitutes `~` for `$HOME` when applicable.
fn format_cwd(_config: &crate::TuiConfig) -> String {
    use std::path::PathBuf;
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Ok(home) = std::env::var("HOME")
        && let Ok(stripped) = cwd.strip_prefix(&home)
    {
        if stripped.as_os_str().is_empty() {
            return "~".to_string();
        }
        return format!("~/{}", stripped.display());
    }
    cwd.display().to_string()
}

fn render_autocomplete(frame: &mut Frame, state: &AppState, area: Rect) {
    use crate::model::AutocompleteKind;

    let ac = match &state.autocomplete {
        Some(ac) if state.mode == InputMode::Normal && !ac.candidates.is_empty() => ac,
        _ => return,
    };

    let popup_height = (ac.candidates.len() as u16 + 2).min(10);
    // Width: longest candidate (label + optional description) plus
    // border + padding, capped to the available area.
    let max_label = ac
        .candidates
        .iter()
        .map(|c| {
            let desc = c.description.as_deref().map(str::len).unwrap_or(0);
            c.value.chars().count() + desc + 6
        })
        .max()
        .unwrap_or(20);
    let popup_width = (max_label as u16 + 4).min(area.width).max(20);

    // Anchor above the prompt (bottom of scrollback area).
    let popup_y = (area.y + area.height)
        .saturating_sub(prompt_line_count(state) as u16)
        .saturating_sub(popup_height);
    let popup_x = area.x + PROMPT_PREFIX.len() as u16;
    let popup_area = Rect::new(popup_x, popup_y, popup_width, popup_height);

    let title = match ac.kind {
        AutocompleteKind::AtMention => "@mention",
        AutocompleteKind::SlashCommand => "/command",
    };

    let items: Vec<ListItem> = ac
        .candidates
        .iter()
        .enumerate()
        .map(|(i, cand)| {
            let style = if i == ac.selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default().fg(Color::White)
            };
            let marker = if i == ac.selected { "▸ " } else { "  " };
            let head = match ac.kind {
                AutocompleteKind::AtMention => format!("{marker}@{}", cand.value),
                AutocompleteKind::SlashCommand => format!("{marker}{}", cand.value),
            };
            let mut spans = vec![Span::styled(head, style)];
            if let Some(desc) = &cand.description {
                spans.push(Span::styled(
                    format!("  {desc}"),
                    Style::default().fg(Color::DarkGray),
                ));
            }
            ListItem::new(Line::from(spans))
        })
        .collect();

    let list = List::new(items).block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(Color::Cyan))
            .title(title),
    );

    frame.render_widget(Clear, popup_area);
    frame.render_widget(list, popup_area);
}

fn build_render_context(state: &AppState) -> RenderContext {
    let mut agent_colors = HashMap::new();
    for agent in &state.agents {
        agent_colors.insert(agent.name.clone(), agent.color);
    }
    RenderContext { agent_colors }
}
