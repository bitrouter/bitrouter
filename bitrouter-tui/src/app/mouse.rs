use crossterm::event::{MouseButton, MouseEvent, MouseEventKind};
use ratatui::layout::Rect;

use crate::model::SessionBadge;

use super::{App, InputMode};

impl App {
    pub(super) fn handle_mouse(&mut self, event: MouseEvent) {
        let layout = match &self.state.last_layout {
            Some(l) => *l,
            None => return,
        };

        match event.kind {
            MouseEventKind::ScrollUp => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_sub(3);
                    sb.follow = false;
                }
            }
            MouseEventKind::ScrollDown => {
                if let Some(sb) = self.state.active_scrollback_mut() {
                    sb.scroll_offset = sb.scroll_offset.saturating_add(3);
                    sb.follow = false;
                }
            }
            MouseEventKind::Down(MouseButton::Left) => {
                let col = event.column;
                let row = event.row;
                if rect_contains(layout.top_bar, col, row) {
                    self.handle_session_bar_click(col);
                } else if rect_contains(layout.scrollback, col, row)
                    && self.state.mode != InputMode::Permission
                {
                    if let Some(sb) = self.state.active_scrollback_mut() {
                        sb.follow = false;
                    }
                    self.state.mode = InputMode::Scroll;
                }
            }
            _ => {}
        }
    }

    fn handle_session_bar_click(&mut self, col: u16) {
        let mut x: u16 = 0;
        for (i, session) in self.state.session_store.active.iter().enumerate() {
            if i > 0 {
                x += 3; // " | " separator
            }
            // dot + space
            x += 2;
            let label = session
                .title
                .as_deref()
                .unwrap_or(session.agent_id.as_str());
            let name_width = label.chars().count() as u16;
            let badge_width = match &session.badge {
                SessionBadge::None => 0,
                SessionBadge::Unread(n) => format!(" [{n}]").chars().count() as u16,
                SessionBadge::Permission => 2, // " !"
            };
            let session_end = x + name_width + badge_width;
            if col >= x && col < session_end {
                self.switch_session(i);
                return;
            }
            x = session_end;
        }

        // Trailing `+` button: opens the agent picker via slash.
        // The top-bar render writes "  +" after the last tab, so the
        // glyph lives at `x + 2`. Be generous with the hit area.
        let plus_x = x + 2;
        if col >= plus_x && col < plus_x + 2 {
            self.slash_session_new_via_mouse();
        }
    }
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}
