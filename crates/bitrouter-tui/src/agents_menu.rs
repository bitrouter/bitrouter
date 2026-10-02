//! Read-only Agents navigation over an application-supplied inventory.
//!
//! No menu input can produce an execution, attachment or permission effect.

use crossterm::event::{Event, KeyCode, KeyEventKind, KeyModifiers};
use ratatui::Frame;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::agents::{
    AgentAttention, AgentDeckSnapshot, AgentProcessState, AgentRunView, AgentTurnState,
};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq)]
enum Filter {
    #[default]
    All,
    NeedsInput,
    Working,
    Inactive,
}

impl Filter {
    fn matches(self, run: &AgentRunView) -> bool {
        match self {
            Self::All => true,
            Self::NeedsInput => matches!(
                run.attention,
                AgentAttention::Question { .. } | AgentAttention::Permission(_)
            ),
            Self::Working => matches!(
                run.turn,
                AgentTurnState::Submitting | AgentTurnState::Working | AgentTurnState::Cancelling
            ),
            Self::Inactive => !Self::NeedsInput.matches(run) && !Self::Working.matches(run),
        }
    }

    fn next(self, reverse: bool) -> Self {
        match (self, reverse) {
            (Self::All, false) | (Self::Working, true) => Self::NeedsInput,
            (Self::NeedsInput, false) | (Self::Inactive, true) => Self::Working,
            (Self::Working, false) | (Self::All, true) => Self::Inactive,
            _ => Self::All,
        }
    }

    fn label(self) -> &'static str {
        match self {
            Self::All => "All",
            Self::NeedsInput => "Needs input",
            Self::Working => "Working",
            Self::Inactive => "Inactive",
        }
    }
}

/// Menu state persists across visits without touching the conversation editor.
#[derive(Debug, Default)]
pub struct AgentsMenu {
    open: bool,
    filter: Filter,
    query: String,
    searching: bool,
    preview: bool,
    selected: Option<String>,
    selected_index: usize,
    ready: bool,
    error: Option<String>,
}

impl AgentsMenu {
    pub fn is_open(&self) -> bool {
        self.open
    }

    pub fn open(&mut self, snapshot: &AgentDeckSnapshot) {
        self.open = true;
        self.refresh(snapshot);
    }

    pub fn set_error(&mut self, message: Option<String>) {
        self.error = message;
    }

    pub fn receive_snapshot(&mut self, snapshot: &AgentDeckSnapshot) -> bool {
        let changed = !self.ready || self.error.is_some();
        self.ready = true;
        self.error = None;
        self.refresh(snapshot);
        changed
    }

    fn rows<'a>(&self, snapshot: &'a AgentDeckSnapshot) -> Vec<&'a AgentRunView> {
        let query = self.query.to_lowercase();
        snapshot
            .runs
            .iter()
            .filter(|run| {
                self.filter.matches(run)
                    && (query.is_empty()
                        || format!(
                            "{} {} {} {}",
                            run.label, run.agent, run.directory, run.run_id
                        )
                        .to_lowercase()
                        .contains(&query))
            })
            .collect()
    }

    fn refresh(&mut self, snapshot: &AgentDeckSnapshot) {
        let rows = self.rows(snapshot);
        self.selected_index = rows
            .iter()
            .position(|run| self.selected.as_deref() == Some(run.run_id.as_str()))
            .unwrap_or(self.selected_index.min(rows.len().saturating_sub(1)));
        self.selected = rows.get(self.selected_index).map(|run| run.run_id.clone());
        if self.selected.is_none() {
            self.preview = false;
        }
    }

    /// Handle only menu-local input. Ctrl-C remains the host's foreground policy.
    pub fn event(&mut self, event: &Event, snapshot: &AgentDeckSnapshot, page_size: usize) {
        if let Event::Paste(text) = event {
            if self.searching {
                self.query.push_str(text);
                self.refresh(snapshot);
            }
            return;
        }
        let Event::Key(key) = event else {
            return;
        };
        if key.kind == KeyEventKind::Release {
            return;
        }
        if self.searching {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => self.searching = false,
                KeyCode::Backspace => {
                    self.query.pop();
                }
                KeyCode::Char(ch)
                    if !key
                        .modifiers
                        .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
                {
                    self.query.push(ch)
                }
                _ => {}
            }
        } else {
            match key.code {
                KeyCode::Esc if key.kind == KeyEventKind::Press => {
                    if self.preview {
                        self.preview = false;
                    } else {
                        self.open = false;
                    }
                }
                KeyCode::Char('/') => {
                    self.searching = true;
                    self.preview = false;
                }
                KeyCode::Enter => self.preview = self.selected.is_some(),
                KeyCode::Tab | KeyCode::BackTab => {
                    self.filter = self.filter.next(
                        key.code == KeyCode::BackTab || key.modifiers.contains(KeyModifiers::SHIFT),
                    );
                    self.preview = false;
                }
                KeyCode::Up | KeyCode::PageUp => {
                    let count = if key.code == KeyCode::PageUp {
                        page_size
                    } else {
                        1
                    };
                    self.selected_index = self.selected_index.saturating_sub(count);
                    self.selected = None;
                }
                KeyCode::Down | KeyCode::PageDown => {
                    let count = if key.code == KeyCode::PageDown {
                        page_size
                    } else {
                        1
                    };
                    self.selected_index = self.selected_index.saturating_add(count);
                    self.selected = None;
                }
                _ => {}
            }
        }
        self.refresh(snapshot);
    }

    pub fn render(
        &self,
        frame: &mut Frame<'_>,
        area: Rect,
        snapshot: &AgentDeckSnapshot,
        permissions: usize,
    ) {
        let [heading, filters, content, help] = Layout::vertical([
            Constraint::Length(1),
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(1),
        ])
        .areas(area);
        let attention = if permissions > 0 {
            format!(" · {permissions} foreground permission(s) waiting")
        } else {
            String::new()
        };
        let heading_text = if self.searching {
            format!("Agents · search: {}▏{attention}", sanitize(&self.query))
        } else if self.query.is_empty() {
            format!("Agents{attention}")
        } else {
            format!("Agents · search: {}{attention}", sanitize(&self.query))
        };
        frame.render_widget(
            Paragraph::new(heading_text).style(Style::default().add_modifier(Modifier::BOLD)),
            heading,
        );
        let filters_text = [
            Filter::All,
            Filter::NeedsInput,
            Filter::Working,
            Filter::Inactive,
        ]
        .into_iter()
        .map(|filter| {
            let count = snapshot
                .runs
                .iter()
                .filter(|run| filter.matches(run))
                .count();
            Span::styled(
                format!("{} {count}  ", filter.label()),
                if self.filter == filter {
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD)
                } else {
                    Style::default().fg(Color::DarkGray)
                },
            )
        })
        .collect::<Vec<_>>();
        if !self.ready {
            frame.render_widget(
                Paragraph::new("Inventory not yet available")
                    .style(Style::default().fg(Color::DarkGray)),
                filters,
            );
        } else if area.width < 68 {
            let count = snapshot
                .runs
                .iter()
                .filter(|run| self.filter.matches(run))
                .count();
            frame.render_widget(
                Paragraph::new(format!(
                    "{} {count} · {} total",
                    self.filter.label(),
                    snapshot.runs.len()
                ))
                .style(Style::default().fg(Color::Cyan)),
                filters,
            );
        } else {
            frame.render_widget(Paragraph::new(Line::from(filters_text)), filters);
        }
        let rows = self.rows(snapshot);
        let lines = if let Some(error) = &self.error {
            vec![Line::styled(
                format!(
                    "{}{}",
                    if self.ready {
                        "Stale inventory · "
                    } else {
                        ""
                    },
                    sanitize(error)
                ),
                Style::default().fg(Color::Yellow),
            )]
        } else if !self.ready {
            vec![Line::from("Loading agent inventory…")]
        } else if rows.is_empty() {
            vec![Line::from(if snapshot.runs.is_empty() {
                "No supervised agents"
            } else {
                "No agents match this filter"
            })]
        } else if self.preview {
            rows.get(self.selected_index)
                .map(|run| {
                    vec![
                        Line::from(format!("{} · {}", sanitize(&run.label), status(run))),
                        Line::from(format!(
                            "Agent: {} · Run: {}",
                            sanitize(&run.agent),
                            sanitize(&run.run_id)
                        )),
                        Line::from(format!("Directory: {}", sanitize(&run.directory))),
                        Line::from(format!(
                            "Session: {}",
                            sanitize(run.native_session_id.as_deref().unwrap_or("unreported"))
                        )),
                        Line::from(format!(
                            "Route: {} · Cost: {}",
                            sanitize(run.confirmed_route.as_deref().unwrap_or("unreported")),
                            sanitize(run.attributed_cost.as_deref().unwrap_or("unreported"))
                        )),
                    ]
                })
                .unwrap_or_default()
        } else {
            let page = usize::from(content.height).max(1);
            let start = self.selected_index / page * page;
            rows.iter()
                .enumerate()
                .skip(start)
                .take(page)
                .map(|(index, run)| {
                    let selected = index == self.selected_index;
                    let label = sanitize(&run.label);
                    let available =
                        usize::from(content.width).saturating_sub(status(run).width() + 5);
                    let mut width = 0;
                    let label = label
                        .graphemes(true)
                        .take_while(|part| {
                            width += part.width();
                            width <= available
                        })
                        .collect::<String>();
                    let text = format!(
                        "{} {label} · {}",
                        if selected { "›" } else { " " },
                        status(run)
                    );
                    Line::styled(
                        text,
                        if selected {
                            Style::default()
                                .fg(Color::Cyan)
                                .add_modifier(Modifier::BOLD)
                        } else {
                            Style::default()
                        },
                    )
                })
                .collect()
        };
        frame.render_widget(Paragraph::new(lines), content);
        let help_text = if self.searching {
            "Enter / Esc list · type to search"
        } else if self.preview {
            "Esc list · read-only metadata"
        } else if self.selected.is_none() {
            if area.width < 68 {
                "Esc back · / search"
            } else {
                "Esc conversation · / search · Tab filter"
            }
        } else if area.width < 68 {
            "↑↓ · Tab · / · Enter view · Esc back"
        } else {
            "↑↓ select · Tab filter · / search · Enter preview · Esc conversation"
        };
        frame.render_widget(
            Paragraph::new(help_text).style(Style::default().fg(Color::DarkGray)),
            help,
        );
    }
}

fn status(run: &AgentRunView) -> &'static str {
    if Filter::NeedsInput.matches(run) {
        return "needs input";
    }
    match run.process {
        AgentProcessState::Failed => "failed",
        AgentProcessState::Stopped => "stopped",
        AgentProcessState::Interrupted => "interrupted",
        AgentProcessState::Starting => "starting",
        AgentProcessState::Stopping => "stopping",
        AgentProcessState::Running if Filter::Working.matches(run) => "working",
        AgentProcessState::Running => "inactive",
    }
}

fn sanitize(text: &str) -> String {
    text.chars()
        .map(|ch| if ch.is_control() { ' ' } else { ch })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::KeyEvent;
    use ratatui::Terminal;
    use ratatui::backend::TestBackend;

    fn key(code: KeyCode) -> Event {
        Event::Key(KeyEvent::new(code, KeyModifiers::NONE))
    }
    fn snapshot(ids: &[&str]) -> AgentDeckSnapshot {
        AgentDeckSnapshot {
            sequence: 1,
            runs: ids
                .iter()
                .map(|id| AgentRunView::new(*id, *id, "stub", "/work"))
                .collect(),
            new_run_target: None,
        }
    }

    #[test]
    fn refresh_preserves_identity_then_chooses_nearest_survivor() {
        let mut menu = AgentsMenu::default();
        let first = snapshot(&["a", "b", "c"]);
        menu.receive_snapshot(&first);
        menu.open(&first);
        menu.event(&key(KeyCode::Down), &first, 3);
        assert_eq!(menu.selected.as_deref(), Some("b"));
        menu.receive_snapshot(&snapshot(&["c", "a", "b"]));
        assert_eq!(menu.selected.as_deref(), Some("b"));
        menu.receive_snapshot(&snapshot(&["c", "a"]));
        assert_eq!(menu.selected.as_deref(), Some("a"));
    }

    #[test]
    fn search_preview_and_return_do_not_touch_snapshot_or_run_authority() {
        let inventory = snapshot(&["审查", "worker"]);
        let original = inventory.clone();
        let mut menu = AgentsMenu::default();
        menu.receive_snapshot(&inventory);
        menu.open(&inventory);
        menu.event(&key(KeyCode::Char('/')), &inventory, 3);
        menu.event(&Event::Paste("审查".to_string()), &inventory, 3);
        menu.event(&key(KeyCode::Enter), &inventory, 3);
        assert_eq!(menu.rows(&inventory).len(), 1);
        menu.event(&key(KeyCode::Enter), &inventory, 3);
        assert!(menu.preview);
        for code in ['r', 's', 'c', 'n'] {
            menu.event(&key(KeyCode::Char(code)), &inventory, 3);
        }
        assert_eq!(inventory, original);
        menu.event(&key(KeyCode::Char('/')), &inventory, 3);
        menu.event(&key(KeyCode::Esc), &inventory, 3);
        assert!(!menu.preview);
        menu.event(&key(KeyCode::Enter), &inventory, 3);
        menu.event(&key(KeyCode::Esc), &inventory, 3);
        assert!(menu.is_open());
        menu.event(&key(KeyCode::Esc), &inventory, 3);
        assert!(!menu.is_open());
        menu.open(&inventory);
        assert_eq!(menu.query, "审查");
    }

    #[test]
    fn minimum_menu_retains_state_and_return_controls() -> std::io::Result<()> {
        let mut inventory = snapshot(&["an extremely long title that must not hide run status"]);
        inventory.runs[0].turn = AgentTurnState::Working;
        let mut menu = AgentsMenu::default();
        menu.receive_snapshot(&inventory);
        menu.open(&inventory);
        let mut terminal = Terminal::new(TestBackend::new(40, 6))?;
        terminal.draw(|frame| menu.render(frame, frame.area(), &inventory, 0))?;
        let screen = terminal
            .backend()
            .buffer()
            .content
            .iter()
            .map(|cell| cell.symbol())
            .collect::<String>();
        assert!(screen.contains("working"));
        assert!(screen.contains("Esc back"));
        assert!(screen.contains("Enter view"));
        Ok(())
    }
}
