use std::collections::HashMap;

use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Paragraph, Scrollbar, ScrollbarOrientation, ScrollbarState};

use crate::TuiConfig;
use crate::app::{AppState, InputMode};
use crate::model::{AutocompleteKind, AutocompleteState, PickerState, RenderContext, Renderable};

const PROMPT_PREFIX: &str = "› ";

/// How many autocomplete candidates we render inline at once. The
/// cursor row is kept inside the window — extra candidates scroll.
const AUTOCOMPLETE_INLINE_ROWS: usize = 8;

/// How many picker items we render inline at once. Same scrolling
/// rule as autocomplete.
const PICKER_INLINE_ROWS: usize = 12;

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    if area.width == 0 || area.height == 0 {
        return;
    }
    let width = area.width;
    let ctx = build_render_context(state);

    let has_scrollback = state
        .active_scrollback()
        .is_some_and(|sb| !sb.entries.is_empty());

    // Empty-state hints render at the top of the region.
    let empty_lines = build_empty_lines(state, has_scrollback);

    // Autocomplete / picker overlay. Rendered inline between the
    // entries and the prompt block — same flow as messages, no
    // floating bordered popup. (Codex's slash-autocomplete style.)
    let overlay_lines = build_overlay_lines(state);

    if let Some(sb) = state.active_scrollback_mut() {
        if sb.cached_width != width {
            sb.invalidate_all();
            sb.cached_width = width;
        }

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
        let prompt_lines = prompt_line_count(state);
        let total = empty_lines.len() + total_entry_lines + overlay_lines.len() + prompt_lines;
        let visible = area.height as usize;

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

    let visible = area.height as usize;
    let scroll_offset = state
        .active_scrollback()
        .map(|sb| sb.scroll_offset)
        .unwrap_or(0);

    let mut lines: Vec<Line> = Vec::new();

    let empty_count = empty_lines.len();
    let entry_total = state
        .active_scrollback()
        .map(|sb| sb.line_offsets.last().copied().unwrap_or(0))
        .unwrap_or(0);
    let overlay_count = overlay_lines.len();
    let prompt_count = prompt_line_count(state);

    let vp_start = scroll_offset;
    let vp_end = scroll_offset + visible;

    // 1. Empty-state lines.
    if !empty_lines.is_empty() && vp_start < empty_count {
        let start = vp_start;
        let end = vp_end.min(empty_count);
        lines.extend(empty_lines.into_iter().skip(start).take(end - start));
    }

    // 2. Entry lines (viewport-aware).
    if entry_total > 0 {
        let entry_global_start = empty_count;
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

                        if scroll_cursor == Some(entry_idx) {
                            apply_cursor_highlight(&mut entry_lines);
                        }

                        let entry_start = sb.line_offsets.get(entry_idx).copied().unwrap_or(0);

                        let skip = local_vp_start.saturating_sub(entry_start);
                        let take = local_vp_end.saturating_sub(entry_start.max(local_vp_start));
                        lines.extend(entry_lines.into_iter().skip(skip).take(take));
                    }
                }
            }
        }
    }

    // 3. Inline overlay (autocomplete or picker).
    if !overlay_lines.is_empty() {
        let overlay_global_start = empty_count + entry_total;
        if vp_end > overlay_global_start && vp_start < overlay_global_start + overlay_count {
            let local_start = vp_start.saturating_sub(overlay_global_start);
            let local_end = (vp_end - overlay_global_start).min(overlay_count);
            lines.extend(
                overlay_lines
                    .into_iter()
                    .skip(local_start)
                    .take(local_end - local_start),
            );
        }
    }

    // 4. Inline prompt — floats below content.
    let prompt_global_start = empty_count + entry_total + overlay_count;
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

    // Cursor: 3 prompt header rows (blank + cwd + divider) precede
    // the input rows.
    if state.mode == InputMode::Normal {
        let total = empty_count + entry_total + overlay_count + prompt_count;
        let prompt_start_line = total.saturating_sub(prompt_count);
        let cursor_line = prompt_start_line + 3 + state.input.cursor.0;
        let cursor_col = if state.input.cursor.0 == 0 {
            PROMPT_PREFIX.chars().count() + state.input.cursor.1
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

    let total = empty_count + entry_total + overlay_count + prompt_count;
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
}

fn build_empty_lines(state: &AppState, has_scrollback: bool) -> Vec<Line<'static>> {
    let mut empty_lines: Vec<Line<'static>> = Vec::new();
    if has_scrollback {
        return empty_lines;
    }
    if state.session_store.active.is_empty() {
        empty_lines.push(Line::raw(""));
        empty_lines.push(Line::from(Span::styled(
            "  Welcome to BitRouter.",
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD),
        )));
        empty_lines.push(Line::raw(""));
        empty_lines.push(Line::from(Span::styled(
            "  Try:",
            Style::default().fg(Color::DarkGray),
        )));
        empty_lines.push(Line::from(Span::styled(
            "    /session new   — spawn a session (opens agent picker)",
            Style::default().fg(Color::DarkGray),
        )));
        empty_lines.push(Line::from(Span::styled(
            "    /agents        — see what's installed / available",
            Style::default().fg(Color::DarkGray),
        )));
        empty_lines.push(Line::from(Span::styled(
            "    /help          — full command reference",
            Style::default().fg(Color::DarkGray),
        )));
    } else {
        let agent_name = state.active_agent_name().unwrap_or("agent");
        empty_lines.push(Line::raw(""));
        empty_lines.push(Line::from(Span::styled(
            format!("  Connected to {agent_name} — type a message to start"),
            Style::default().fg(Color::DarkGray),
        )));
    }
    empty_lines
}

/// Render the active autocomplete or picker as inline rows, to be
/// placed between the entries and the prompt block. Returns an empty
/// vec when neither is active. Both look like ordinary scrollback
/// content — no border, no `Clear`.
fn build_overlay_lines(state: &AppState) -> Vec<Line<'static>> {
    if state.mode == InputMode::Picker
        && let Some(picker) = &state.picker
    {
        return render_picker_inline(picker);
    }
    if state.mode == InputMode::Normal
        && let Some(ac) = &state.autocomplete
        && !ac.candidates.is_empty()
    {
        return render_autocomplete_inline(ac);
    }
    Vec::new()
}

fn render_autocomplete_inline(ac: &AutocompleteState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let total = ac.candidates.len();
    let visible = AUTOCOMPLETE_INLINE_ROWS.min(total);
    let start = window_start(ac.selected, visible, total);
    let end = (start + visible).min(total);

    lines.push(Line::raw(""));
    for i in start..end {
        let cand = &ac.candidates[i];
        let selected = i == ac.selected;
        let marker = if selected { "▸ " } else { "  " };
        let value_style = if selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let head = match ac.kind {
            AutocompleteKind::AtMention => format!("  {marker}@{}", cand.value),
            AutocompleteKind::SlashCommand => format!("  {marker}{}", cand.value),
        };
        let mut spans = vec![Span::styled(head, value_style)];
        if let Some(desc) = &cand.description {
            spans.push(Span::styled(
                format!("  {desc}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }
    lines
}

fn render_picker_inline(picker: &PickerState) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let total = picker.items.len();
    let visible = PICKER_INLINE_ROWS.min(total);
    let start = window_start(picker.cursor, visible, total);
    let end = (start + visible).min(total);

    lines.push(Line::raw(""));
    lines.push(Line::from(Span::styled(
        format!("  {}", picker.title),
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD),
    )));

    for i in start..end {
        let item = &picker.items[i];
        let cursor_marker = if i == picker.cursor && item.selectable {
            "▸ "
        } else {
            "  "
        };
        let checkbox = if picker.multiselect && item.selectable {
            if picker.selected.contains(&i) {
                "[x] "
            } else {
                "[ ] "
            }
        } else {
            ""
        };
        let label_style = if !item.selectable {
            Style::default().fg(Color::DarkGray)
        } else if i == picker.cursor {
            Style::default()
                .fg(Color::White)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        let mut spans: Vec<Span<'static>> = vec![
            Span::raw("  "),
            Span::styled(cursor_marker, Style::default().fg(Color::Cyan)),
            Span::styled(
                checkbox.to_string(),
                if picker.multiselect && picker.selected.contains(&i) {
                    Style::default().fg(Color::Green)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            ),
            Span::styled(item.label.clone(), label_style),
        ];
        if let Some(sub) = &item.subtitle {
            spans.push(Span::styled(
                format!("  {sub}"),
                Style::default().fg(Color::DarkGray),
            ));
        }
        lines.push(Line::from(spans));
    }

    let hint = if picker.multiselect {
        "  ↑↓ select · Space toggle · Enter confirm · Esc cancel"
    } else {
        "  ↑↓ select · Enter confirm · Esc cancel"
    };
    lines.push(Line::from(Span::styled(
        hint,
        Style::default().fg(Color::DarkGray),
    )));
    lines
}

/// Pick the start of a `visible`-sized window over `total` items so
/// `cursor` is roughly centered. Returns 0 when everything fits.
fn window_start(cursor: usize, visible: usize, total: usize) -> usize {
    if total <= visible || cursor < visible / 2 {
        0
    } else if cursor + visible / 2 >= total {
        total.saturating_sub(visible)
    } else {
        cursor.saturating_sub(visible / 2)
    }
}

fn visible_entry_range(line_offsets: &[usize], vp_start: usize, vp_end: usize) -> (usize, usize) {
    if line_offsets.len() < 2 {
        return (0, 0);
    }
    let entry_count = line_offsets.len() - 1;

    let first = line_offsets
        .partition_point(|&o| o <= vp_start)
        .saturating_sub(1)
        .min(entry_count.saturating_sub(1));

    let last = line_offsets
        .partition_point(|&o| o < vp_end)
        .min(entry_count);

    (first, last)
}

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

    let focused = state.mode == InputMode::Normal;
    let prompt_style = if focused {
        Style::default()
            .fg(Color::Cyan)
            .add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    for (i, line) in state.input.lines.iter().enumerate() {
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

/// Total rows the inline prompt block occupies: 1 blank spacer + 1
/// cwd label + 1 divider + N input rows.
fn prompt_line_count(state: &AppState) -> usize {
    3 + state.input.lines.len()
}

fn format_cwd(_config: &TuiConfig) -> String {
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

fn build_render_context(state: &AppState) -> RenderContext {
    let mut agent_colors = HashMap::new();
    for agent in &state.agents {
        agent_colors.insert(agent.name.clone(), agent.color);
    }
    RenderContext { agent_colors }
}
