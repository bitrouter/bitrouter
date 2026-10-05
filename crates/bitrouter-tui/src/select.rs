//! Searchable, bounded single-choice lists. Callers supply rows and own input;
//! selection returns the original row index, even while the list is filtered.

use std::io;

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, List, ListItem, ListState, Paragraph};
use ratatui::{Frame, Terminal};

const ROWS: usize = 8;

/// A selectable label and its searchable context (for example a provider id).
pub struct Item {
    pub label: String,
    pub detail: String,
}

impl Item {
    pub fn new(label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Pending,
    Selected(usize),
    Cancelled,
}

/// Keyboard/search state, independent of terminal I/O and application data.
pub struct Select {
    items: Vec<Item>,
    query: String,
    matches: Vec<usize>,
    list: ListState,
    page_size: usize,
}

impl Select {
    pub fn new(items: Vec<Item>, selected: usize) -> Self {
        let len = items.len();
        Self {
            items,
            query: String::new(),
            matches: (0..len).collect(),
            list: ListState::default()
                .with_selected((len > 0).then_some(selected.min(len.saturating_sub(1)))),
            page_size: ROWS,
        }
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.matches = self
            .items
            .iter()
            .enumerate()
            .filter(|(_, item)| {
                format!("{} {}", item.label, item.detail)
                    .to_lowercase()
                    .contains(&query)
            })
            .map(|(index, _)| index)
            .collect();
        self.list = ListState::default().with_selected((!self.matches.is_empty()).then_some(0));
    }

    fn move_by(&mut self, delta: isize) {
        if let Some(selected) = self.list.selected() {
            self.list.select(Some(
                selected
                    .saturating_add_signed(delta)
                    .min(self.matches.len().saturating_sub(1)),
            ));
        }
    }

    /// Digits are search text. Only Enter accepts a row; Esc/Ctrl-C cancel.
    pub fn handle(&mut self, event: Event) -> Action {
        match event {
            Event::Paste(text) => {
                self.query.extend(text.chars().filter(|c| !c.is_control()));
                self.filter();
            }
            Event::Key(key) if key.kind != KeyEventKind::Release => {
                if key.modifiers.contains(KeyModifiers::CONTROL) {
                    match key.code {
                        KeyCode::Char('c' | 'd') => return Action::Cancelled,
                        KeyCode::Char('u') => {
                            self.query.clear();
                            self.filter();
                        }
                        _ => {}
                    }
                    return Action::Pending;
                }
                match key.code {
                    KeyCode::Esc => return Action::Cancelled,
                    KeyCode::Enter => {
                        if let Some(index) = self
                            .list
                            .selected()
                            .and_then(|selected| self.matches.get(selected))
                        {
                            return Action::Selected(*index);
                        }
                    }
                    KeyCode::Up => self.move_by(-1),
                    KeyCode::Down => self.move_by(1),
                    KeyCode::PageUp => self.move_by(-(self.page_size as isize)),
                    KeyCode::PageDown => self.move_by(self.page_size as isize),
                    KeyCode::Home => self.move_by(-(self.matches.len() as isize)),
                    KeyCode::End => self.move_by(self.matches.len() as isize),
                    KeyCode::Backspace => {
                        self.query.pop();
                        self.filter();
                    }
                    KeyCode::Char(c) if !key.modifiers.contains(KeyModifiers::ALT) => {
                        self.query.push(c);
                        self.filter();
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        Action::Pending
    }

    fn render(&mut self, frame: &mut Frame<'_>, title: &str, help: &str) {
        let area = frame.area();
        // Keep the list height steady as results narrow, but fit small terminals.
        self.page_size = ROWS.min(usize::from(area.height.saturating_sub(8))).max(1);
        let area = Rect::new(area.x, area.y, area.width.min(100), area.height);
        let [heading, search, list, footer] = Layout::vertical([
            Constraint::Length(2),
            Constraint::Length(3),
            Constraint::Length(self.page_size as u16 + 2),
            Constraint::Min(1),
        ])
        .areas(area);
        frame.render_widget(
            Paragraph::new(vec![
                Line::styled(
                    title,
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ),
                Line::from(help),
            ]),
            heading,
        );
        let query = if self.query.is_empty() {
            "Type to search…".to_string()
        } else {
            self.query.clone()
        };
        // Keep the end of a long query visible, using terminal cell widths.
        let width = usize::from(search.width.saturating_sub(2));
        let mut tail = String::new();
        let mut cells = 0;
        for c in query.chars().rev() {
            cells += unicode_width::UnicodeWidthChar::width(c).unwrap_or(0);
            if cells > width {
                break;
            }
            tail.push(c);
        }
        frame.render_widget(
            Paragraph::new(tail.chars().rev().collect::<String>())
                .block(Block::bordered().title(" Search ")),
            search,
        );
        let position = self.list.selected().map_or(0, |index| index + 1);
        let block = Block::bordered().title(format!(" {position}/{} ", self.matches.len()));
        if self.matches.is_empty() {
            frame.render_widget(
                Paragraph::new("No matches — Ctrl-U clears search").block(block),
                list,
            );
        } else {
            let rows: Vec<_> = self
                .matches
                .iter()
                .map(|index| {
                    let item = &self.items[*index];
                    ListItem::new(Line::from(vec![
                        Span::raw(item.label.clone()),
                        Span::styled(
                            format!("  {}", item.detail),
                            Style::default().fg(Color::DarkGray),
                        ),
                    ]))
                })
                .collect();
            frame.render_stateful_widget(
                List::new(rows)
                    .block(block)
                    .highlight_symbol("› ")
                    .highlight_style(
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    ),
                list,
                &mut self.list,
            );
        }
        frame.render_widget(
            Paragraph::new(
                "↑/↓ move · Enter select · Esc cancel\nPgUp/PgDn scroll · Home/End · Ctrl-U clear",
            ),
            footer,
        );
    }
}

/// A temporary selection surface on stderr; stdout stays available for JSON.
/// Dropping it restores the terminal before a login flow or chat takes over.
pub struct View {
    terminal: Terminal<CrosstermBackend<io::Stderr>>,
}

impl View {
    pub fn open() -> io::Result<Self> {
        let terminal = Terminal::new(CrosstermBackend::new(io::stderr()))?;
        crate::lifecycle::enter_raw()?;
        let mut view = Self { terminal };
        crossterm::execute!(
            view.terminal.backend_mut(),
            crossterm::terminal::EnterAlternateScreen,
            crossterm::event::EnableBracketedPaste,
            crossterm::cursor::Hide
        )?;
        view.terminal.clear()?;
        Ok(view)
    }

    pub fn draw(&mut self, select: &mut Select, title: &str, help: &str) -> io::Result<()> {
        let size = self.terminal.size()?;
        if size.width < 24 || size.height < 9 {
            return Err(io::Error::other(
                "selection needs a terminal at least 24 columns by 9 rows",
            ));
        }
        self.terminal
            .draw(|frame| select.render(frame, title, help))?;
        Ok(())
    }
}

impl Drop for View {
    fn drop(&mut self) {
        let _ = crossterm::terminal::disable_raw_mode();
        let _ = crossterm::execute!(
            self.terminal.backend_mut(),
            crossterm::event::DisableBracketedPaste,
            crossterm::terminal::LeaveAlternateScreen,
            crossterm::cursor::Show
        );
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::backend::TestBackend;

    fn key(select: &mut Select, code: KeyCode) -> Action {
        select.handle(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn many() -> Select {
        Select::new(
            (0..45)
                .map(|i| Item::new(format!("Provider {i:02}"), format!("id-{i}")))
                .collect(),
            0,
        )
    }

    #[test]
    fn search_selects_original_indices_and_digits_never_choose() {
        let mut select = many();
        assert_eq!(key(&mut select, KeyCode::Char('4')), Action::Pending);
        key(&mut select, KeyCode::Down);
        assert_eq!(key(&mut select, KeyCode::Enter), Action::Selected(14));
        select.handle(Event::Paste("no-match".into()));
        assert_eq!(key(&mut select, KeyCode::Enter), Action::Pending);
        select.handle(Event::Key(KeyEvent::new(
            KeyCode::Char('u'),
            KeyModifiers::CONTROL,
        )));
        key(&mut select, KeyCode::End);
        assert_eq!(key(&mut select, KeyCode::Enter), Action::Selected(44));
        assert_eq!(key(&mut select, KeyCode::Esc), Action::Cancelled);
    }

    #[test]
    fn navigation_reaches_every_row_and_stays_in_bounds() {
        let mut select = many();
        key(&mut select, KeyCode::Up);
        for index in 0..45 {
            assert_eq!(key(&mut select, KeyCode::Enter), Action::Selected(index));
            key(&mut select, KeyCode::Down);
        }
        assert_eq!(key(&mut select, KeyCode::Enter), Action::Selected(44));
        key(&mut select, KeyCode::Home);
        key(&mut select, KeyCode::PageDown);
        assert_eq!(key(&mut select, KeyCode::Enter), Action::Selected(8));
        key(&mut select, KeyCode::PageUp);
        assert_eq!(key(&mut select, KeyCode::Enter), Action::Selected(0));
        let mut empty = Select::new(vec![], 10);
        key(&mut empty, KeyCode::End);
        assert_eq!(key(&mut empty, KeyCode::Enter), Action::Pending);
    }

    #[test]
    fn rendering_scrolls_and_resizes_without_losing_selection() -> io::Result<()> {
        let mut select = many();
        let mut terminal = Terminal::new(TestBackend::new(70, 20))?;
        key(&mut select, KeyCode::End);
        terminal.draw(|frame| select.render(frame, "Providers", "Choose a provider"))?;
        let screen = format!("{:?}", terminal.backend().buffer());
        assert!(screen.contains("› Provider 44"));
        assert!(!screen.contains("Provider 00"));
        assert_eq!(select.page_size, ROWS);
        terminal.backend_mut().resize(30, 10);
        terminal.draw(|frame| select.render(frame, "Providers", "Choose a provider"))?;
        assert!(format!("{:?}", terminal.backend().buffer()).contains("› Provider 44"));
        assert_eq!(select.page_size, 2);
        assert_eq!(key(&mut select, KeyCode::Enter), Action::Selected(44));
        Ok(())
    }

    #[test]
    fn unicode_search_backspace_and_paste_are_safe() -> io::Result<()> {
        let mut select = Select::new(vec![Item::new("云 Provider", "cloud")], 0);
        select.handle(Event::Paste("云\n".into()));
        assert_eq!(key(&mut select, KeyCode::Enter), Action::Selected(0));
        key(&mut select, KeyCode::Backspace);
        assert!(select.query.is_empty());
        select.handle(Event::Paste("云".repeat(50)));
        let mut terminal = Terminal::new(TestBackend::new(24, 9))?;
        terminal.draw(|frame| select.render(frame, "Providers", "Choose"))?;
        assert!(format!("{:?}", terminal.backend().buffer()).contains("No matches"));
        Ok(())
    }
}
