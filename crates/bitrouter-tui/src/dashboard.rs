//! Full-screen operations dashboard renderer.
//!
//! The application owns every read and every async effect. This module accepts
//! plain display data, owns terminal lifecycle, and renders it; it cannot reach
//! BitRouter config, HTTP, IPC, or the metering store.

use std::io::{self, IsTerminal};

use crossterm::cursor::Hide;
use crossterm::execute;
use crossterm::terminal::EnterAlternateScreen;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::{Frame, Terminal};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Overview,
    Models,
    Requests,
    Route,
}

impl Page {
    const ALL: [Self; 4] = [Self::Overview, Self::Models, Self::Requests, Self::Route];

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn index(self) -> usize {
        match self {
            Self::Overview => 0,
            Self::Models => 1,
            Self::Requests => 2,
            Self::Route => 3,
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct Dashboard {
    pub target: String,
    pub connected: bool,
    pub status: String,
    pub pid: Option<u32>,
    pub listen: Option<String>,
    pub providers: Vec<String>,
    pub spend: Option<String>,
    pub models: Vec<ModelLine>,
    pub requests: Vec<RequestLine>,
    pub route_input: String,
    pub route: Option<RouteLine>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ModelLine {
    pub id: String,
    pub providers: String,
}

#[derive(Debug, Clone)]
pub struct RequestLine {
    pub time: String,
    pub model: String,
    pub provider: String,
    pub tokens: String,
    pub cost: String,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct RouteLine {
    pub requested: String,
    pub effective: String,
    pub providers: String,
    pub resolved_via: String,
}

pub struct DashboardView {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    page: Page,
    finished: bool,
}

impl DashboardView {
    pub fn open() -> io::Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(io::Error::other(
                "bitrouter tui requires an interactive stdin and stdout",
            ));
        }
        crate::lifecycle::install_panic_restore();
        crate::lifecycle::enter_raw()?;
        let mut stdout = std::io::stdout();
        if let Err(error) = execute!(stdout, EnterAlternateScreen, Hide) {
            crate::lifecycle::restore();
            return Err(error);
        }
        let terminal = match Terminal::new(CrosstermBackend::new(stdout)) {
            Ok(terminal) => terminal,
            Err(error) => {
                crate::lifecycle::restore();
                return Err(error);
            }
        };
        Ok(Self {
            terminal,
            page: Page::Overview,
            finished: false,
        })
    }

    pub fn page(&self) -> Page {
        self.page
    }

    pub fn set_page(&mut self, page: Page) {
        self.page = page;
    }

    pub fn next_page(&mut self) {
        self.page = self.page.next();
    }

    pub fn draw(&mut self, dashboard: &Dashboard) -> io::Result<()> {
        let page = self.page;
        self.terminal
            .draw(|frame| draw(frame, page, dashboard))
            .map(|_| ())
    }

    pub fn finish(&mut self) {
        if !self.finished {
            crate::lifecycle::restore();
            self.finished = true;
        }
    }
}

impl Drop for DashboardView {
    fn drop(&mut self) {
        self.finish();
    }
}

fn draw(frame: &mut Frame<'_>, page: Page, dashboard: &Dashboard) {
    let area = frame.area();
    let error_height = u16::from(dashboard.error.is_some()) * 3;
    let [header, content, error, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(error_height),
        Constraint::Length(1),
    ])
    .areas(area);

    let titles = ["Overview", "Models", "Requests", "Route"]
        .into_iter()
        .map(Line::from);
    let tabs = Tabs::new(titles)
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(format!(" BitRouter · {} ", dashboard.target)),
        )
        .select(page.index())
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("│");
    frame.render_widget(tabs, header);

    match page {
        Page::Overview => draw_overview(frame, content, dashboard),
        Page::Models => draw_models(frame, content, dashboard),
        Page::Requests => draw_requests(frame, content, dashboard),
        Page::Route => draw_route(frame, content, dashboard),
    }

    if let Some(message) = &dashboard.error {
        frame.render_widget(
            Paragraph::new(message.as_str())
                .style(Style::default().fg(Color::Red))
                .block(Block::default().borders(Borders::TOP).title(" Error "))
                .wrap(Wrap { trim: true }),
            error,
        );
    }

    let help = if page == Page::Route {
        "Tab pages · type model · Enter preview · Ctrl-U clear · Esc/Ctrl-C quit"
    } else {
        "Tab pages · r refresh · 1-4 jump · q/Esc/Ctrl-C quit"
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

fn draw_overview(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let health = if dashboard.connected {
        Span::styled("● connected", Style::default().fg(Color::Green))
    } else {
        Span::styled("○ unavailable", Style::default().fg(Color::Red))
    };
    let mut lines = vec![Line::from(health), Line::from("")];
    lines.push(field("status", &dashboard.status));
    if let Some(pid) = dashboard.pid {
        lines.push(field("pid", &pid.to_string()));
    }
    if let Some(listen) = &dashboard.listen {
        lines.push(field("inference", listen));
    }
    lines.push(field("models", &dashboard.models.len().to_string()));
    if !dashboard.providers.is_empty() {
        lines.push(field("providers", &dashboard.providers.join(", ")));
    }
    if let Some(spend) = &dashboard.spend {
        lines.push(field("spend", spend));
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Status "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_models(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let rows = dashboard.models.iter().map(|model| {
        Row::new(vec![
            Cell::from(model.id.clone()),
            Cell::from(model.providers.clone()),
        ])
    });
    let table = Table::new(
        rows,
        [Constraint::Percentage(45), Constraint::Percentage(55)],
    )
    .header(Row::new(["MODEL", "PROVIDERS"]).style(Style::default().add_modifier(Modifier::BOLD)))
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Models · {} ", dashboard.models.len())),
    );
    frame.render_widget(table, area);
}

fn draw_requests(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let rows = dashboard.requests.iter().map(|request| {
        Row::new(vec![
            request.time.clone(),
            request.model.clone(),
            request.provider.clone(),
            request.tokens.clone(),
            request.cost.clone(),
            request.status.clone(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(10),
            Constraint::Percentage(30),
            Constraint::Percentage(22),
            Constraint::Length(10),
            Constraint::Length(11),
            Constraint::Percentage(20),
        ],
    )
    .header(
        Row::new(["TIME", "MODEL", "PROVIDER", "TOKENS", "COST", "STATUS"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(format!(" Recent requests · {} ", dashboard.requests.len())),
    );
    frame.render_widget(table, area);
}

fn draw_route(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let [input, result] = Layout::vertical([Constraint::Length(3), Constraint::Min(3)]).areas(area);
    frame.render_widget(
        Paragraph::new(dashboard.route_input.as_str()).block(
            Block::default()
                .borders(Borders::ALL)
                .title(" Model selector "),
        ),
        input,
    );
    let lines = match &dashboard.route {
        Some(route) => vec![
            field("requested", &route.requested),
            field("effective", &route.effective),
            field("providers", &route.providers),
            field("resolved via", &route.resolved_via),
        ],
        None => vec![Line::from("Type a model selector and press Enter.")],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Preview "))
            .wrap(Wrap { trim: false }),
        result,
    );
}

fn field(name: &str, value: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            format!("{name:>12}  "),
            Style::default().fg(Color::DarkGray),
        ),
        Span::raw(value.to_string()),
    ])
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::backend::TestBackend;

    #[test]
    fn every_page_renders_with_sparse_data() -> io::Result<()> {
        let backend = TestBackend::new(80, 24);
        let mut terminal = Terminal::new(backend)?;
        let dashboard = Dashboard {
            target: "remote:workstation".to_string(),
            status: "running".to_string(),
            ..Dashboard::default()
        };
        for page in Page::ALL {
            terminal.draw(|frame| draw(frame, page, &dashboard))?;
        }
        Ok(())
    }
}
