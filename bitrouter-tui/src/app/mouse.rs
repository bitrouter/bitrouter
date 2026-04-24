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
            let name_width = session.agent_name.chars().count() as u16;
            let badge_width = match &session.badge {
                SessionBadge::None => 0,
                SessionBadge::Unread(n) => format!(" [{n}]").chars().count() as u16,
                SessionBadge::Permission => 2, // " !"
            };
            let session_end = x + name_width + badge_width;
            if col >= x && col < session_end {
                self.switch_session(i);
                if self.state.mode == InputMode::Tab {
                    self.state.mode = InputMode::Normal;
                }
                return;
            }
            x = session_end;
        }
    }
}

fn rect_contains(rect: Rect, col: u16, row: u16) -> bool {
    col >= rect.x && col < rect.x + rect.width && row >= rect.y && row < rect.y + rect.height
}
