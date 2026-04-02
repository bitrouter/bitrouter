use ratatui::Frame;
use ratatui::layout::Rect;
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, List, ListItem};

use crate::app::{AppState, Focus};
use crate::model::InputTarget;

pub fn render(frame: &mut Frame, state: &mut AppState, area: Rect) {
    let focused = state.focus == Focus::Input;
    let has_permission = state.feed.entries.iter().any(|e| {
        matches!(
            &e.kind,
            crate::model::EntryKind::PermissionRequest {
                resolved: false,
                ..
            }
        )
    });

    let border_style = if has_permission {
        Style::default().fg(Color::Yellow)
    } else if focused {
        Style::default().fg(Color::Cyan)
    } else {
        Style::default().fg(Color::DarkGray)
    };

    // Build title with target indicator.
    let target_label = match &state.input_target {
        InputTarget::Default => {
            if let Some(default) = &state.default_agent {
                format!("Input → {default}")
            } else {
                "Input".to_string()
            }
        }
        InputTarget::Specific(names) => {
            format!("Input → {}", names.join(", "))
        }
        InputTarget::All => "Input → @all".to_string(),
    };

    state.input.set_block(
        Block::default()
            .borders(Borders::ALL)
            .border_style(border_style)
            .title(target_label),
    );
    state.input.set_cursor_line_style(Style::default());

    if focused && !has_permission {
        state
            .input
            .set_cursor_style(Style::default().bg(Color::White).fg(Color::Black));
    } else {
        state.input.set_cursor_style(Style::default());
    }

    frame.render_widget(&state.input, area);

    // Autocomplete popup — rendered as floating list above the input bar.
    if let Some(ac) = &state.autocomplete
        && focused
        && !ac.candidates.is_empty()
    {
        let popup_height = (ac.candidates.len() as u16 + 2).min(8);
        let popup_width = 24u16.min(area.width);
        let popup_y = area.y.saturating_sub(popup_height);
        let popup_area = Rect::new(area.x + 1, popup_y, popup_width, popup_height);

        let items: Vec<ListItem> = ac
            .candidates
            .iter()
            .enumerate()
            .map(|(i, name)| {
                let style = if i == ac.selected {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::White)
                };
                let marker = if i == ac.selected { "▸ " } else { "  " };
                ListItem::new(Line::from(Span::styled(format!("{marker}@{name}"), style)))
            })
            .collect();

        let list = List::new(items).block(
            Block::default()
                .borders(Borders::ALL)
                .border_style(Style::default().fg(Color::Cyan))
                .title("@mention"),
        );

        frame.render_widget(Clear, popup_area);
        frame.render_widget(list, popup_area);
    }
}
