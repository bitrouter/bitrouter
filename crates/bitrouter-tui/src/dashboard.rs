//! ACP transcript in native scrollback, with a docked operations shell.
//!
//! The application owns every read and every async effect. This module accepts
//! plain display data, owns terminal lifecycle, and renders it; it cannot reach
//! BitRouter config, HTTP, IPC, or the metering store.

use std::io::{self, IsTerminal};

use crossterm::event::KeyEvent;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::buffer::Buffer;
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Cell, Paragraph, Row, Table, Tabs, Wrap};
use ratatui::{Frame, Terminal};
use unicode_width::UnicodeWidthStr as _;

use crate::editor::{Edit, Editor};
use crate::writer::{Cache, Writer};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Page {
    Home,
    Agents,
    Conversation,
    Sessions,
    Models,
    Requests,
    Route,
}

impl Page {
    const ALL: [Self; 7] = [
        Self::Home,
        Self::Agents,
        Self::Conversation,
        Self::Sessions,
        Self::Models,
        Self::Requests,
        Self::Route,
    ];

    pub fn next(self) -> Self {
        Self::ALL[(self.index() + 1) % Self::ALL.len()]
    }

    fn index(self) -> usize {
        match self {
            Self::Home => 0,
            Self::Agents => 1,
            Self::Conversation => 2,
            Self::Sessions => 3,
            Self::Models => 4,
            Self::Requests => 5,
            Self::Route => 6,
        }
    }
}

#[derive(Debug, Default)]
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
    /// Action failure retained until that action later succeeds.
    pub error: Option<String>,
    /// Connection/snapshot failure cleared by the next successful refresh.
    pub refresh_error: Option<String>,
    /// Non-fatal action diagnostic, such as an ACP routing fallback.
    pub notice: Option<String>,
    pub agents: Vec<AgentLine>,
    pub selected_agent: usize,
    pub conversation: Conversation,
}

#[derive(Debug, Clone)]
pub struct AgentLine {
    pub id: String,
    pub native: bool,
    pub acp: bool,
    pub configured: bool,
    pub description: String,
}

/// Presentation state for the one controller/session the Code process owns.
#[derive(Debug, Default)]
pub struct Conversation {
    pub agent: Option<String>,
    pub native_session_id: Option<String>,
    pub provider_session_id: Option<String>,
    pub lifecycle: Option<String>,
    pub route: Option<String>,
    pub status: String,
    pub input: Editor,
    pub journal: crate::journal::Journal,
    pub permission: Option<crate::permission::Prompt>,
}

/// Pure Code-shell state transition. Async launch/prompt effects stay in the
/// application driver; this reducer owns selection and the multiline draft.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SelectPreviousAgent,
    SelectNextAgent,
    ActivateSelectedAgent,
    Key(KeyEvent),
    Paste(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    StartAgent(String),
    Prompt(String),
}

pub fn step(dashboard: &mut Dashboard, action: Action) -> Option<Effect> {
    match action {
        Action::SelectPreviousAgent => {
            dashboard.selected_agent = dashboard.selected_agent.saturating_sub(1);
            None
        }
        Action::SelectNextAgent => {
            dashboard.selected_agent = dashboard
                .selected_agent
                .saturating_add(1)
                .min(dashboard.agents.len().saturating_sub(1));
            None
        }
        Action::ActivateSelectedAgent => dashboard
            .agents
            .get(dashboard.selected_agent)
            .map(|agent| Effect::StartAgent(agent.id.clone())),
        Action::Key(key) => {
            if dashboard.conversation.input.apply(key) == Edit::Submitted {
                let prompt = dashboard.conversation.input.take();
                return (!prompt.trim().is_empty()).then_some(Effect::Prompt(prompt));
            }
            None
        }
        Action::Paste(text) => {
            dashboard.conversation.input.paste(&text);
            None
        }
    }
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
    writer: Writer<CrosstermBackend<io::Stdout>>,
    page: Page,
    conversation_cache: Cache,
    conversation_registry: crate::render::Registry,
    session: Option<String>,
    animation: usize,
    finished: bool,
}

impl DashboardView {
    pub fn open() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other(
                "bitrouter tui requires an interactive stdin and stdout",
            ));
        }
        crate::lifecycle::install_panic_restore();
        crate::lifecycle::enter_raw()?;
        let writer = match (|| {
            crate::lifecycle::enable_session_keys()?;
            Writer::new(CrosstermBackend::new(io::stdout()))
        })() {
            Ok(writer) => writer,
            Err(error) => {
                crate::lifecycle::restore();
                return Err(error);
            }
        };
        Ok(Self {
            writer,
            page: Page::Home,
            conversation_cache: Cache::default(),
            conversation_registry: crate::render::Registry::default(),
            session: None,
            animation: 0,
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
    pub fn tick(&mut self) {
        self.animation = (self.animation + 1) % SPINNER.len();
    }
    pub fn invalidate(&mut self) {
        self.writer.invalidate();
    }

    pub fn draw(&mut self, dashboard: &mut Dashboard) -> io::Result<()> {
        if self.session != dashboard.conversation.native_session_id {
            self.writer.new_document()?;
            self.conversation_cache = Cache::default();
            self.session
                .clone_from(&dashboard.conversation.native_session_id);
        }
        let size = self.writer.size();
        let padding = padding(size.width);
        let width = size.width.saturating_sub(padding * 2).max(1);
        dashboard
            .conversation
            .input
            .set_width(width.saturating_sub(2));
        let document = self.conversation_cache.document(
            &dashboard.conversation.journal,
            &self.conversation_registry,
            Size::new(width, size.height),
            &[],
        );
        let transcript = padded_transcript(&document, size.width);
        let height = dock_height(dashboard, self.page, size);
        // Off-screen ratatui rendering keeps widgets reusable while the writer
        // owns the real terminal and its native scrollback.
        let mut dock = Terminal::new(TestBackend::new(size.width.max(1), height))?;
        let mut cursor = None;
        let frame = dock.draw(|frame| {
            cursor = draw(frame, self.page, dashboard, self.animation);
        })?;
        let cursor = cursor.map(|position| {
            Position::new(position.x, size.height.saturating_sub(height) + position.y)
        });
        let footer = buffer_lines(frame.buffer);
        self.writer.docked_frame(&transcript, &footer)?;
        self.writer.cursor(cursor)
    }

    pub fn finish(&mut self) {
        if !self.finished {
            let _ = self.writer.finish();
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

const SPINNER: [&str; 10] = ["⠋", "⠙", "⠹", "⠸", "⠼", "⠴", "⠦", "⠧", "⠇", "⠏"];

fn padding(width: u16) -> u16 {
    width.saturating_sub(4).saturating_div(2).min(2)
}

fn buffer_lines(buffer: &Buffer) -> Vec<Line<'static>> {
    (buffer.area.top()..buffer.area.bottom())
        .map(|y| {
            let mut spans = Vec::new();
            let mut x = buffer.area.left();
            while x < buffer.area.right() {
                let cell = &buffer[(x, y)];
                spans.push(Span::styled(cell.symbol().to_string(), cell.style()));
                x = x.saturating_add(u16::try_from(cell.symbol().width()).unwrap_or(1).max(1));
            }
            Line::from(spans)
        })
        .collect()
}

fn padded_transcript(document: &[Line<'static>], width: u16) -> Vec<Line<'static>> {
    let padding = padding(width);
    let inner = width.saturating_sub(2 * padding).max(1);
    document
        .iter()
        .flat_map(|line| {
            let rows = crate::wrap::wrap(line, inner);
            if rows.iter().all(|row| row.width() <= usize::from(inner)) {
                return rows;
            }
            // The word wrapper can overflow by a wide cluster. Fall back to
            // grapheme wrapping for this line instead of clipping CJK/emoji or
            // painting into the right gutter.
            let mut rows = Vec::new();
            let mut row = Line::default();
            let mut used = 0;
            for grapheme in line.styled_graphemes(Style::default()) {
                let cells = grapheme.symbol.width();
                if used > 0 && used + cells > usize::from(inner) {
                    rows.push(std::mem::take(&mut row));
                    used = 0;
                }
                row.spans
                    .push(Span::styled(grapheme.symbol.to_string(), grapheme.style));
                used += cells;
            }
            rows.push(row);
            rows
        })
        .map(|mut line| {
            line.spans
                .insert(0, Span::raw(" ".repeat(usize::from(padding))));
            line
        })
        .collect()
}

fn composer_height(dashboard: &Dashboard, width: u16, height: u16) -> u16 {
    let rows = dashboard
        .conversation
        .input
        .layout(width.saturating_sub(2))
        .rows
        .len();
    let max_rows = height.saturating_sub(7).clamp(1, 8);
    u16::try_from(rows).unwrap_or(u16::MAX).clamp(1, max_rows) + 2
}

fn dock_height(dashboard: &Dashboard, page: Page, size: Size) -> u16 {
    let width = size.width.saturating_sub(2 * padding(size.width));
    let input = if page == Page::Conversation {
        composer_height(dashboard, width, size.height)
    } else {
        0
    };
    let notice = u16::from(dashboard.notice.is_some()) * 3;
    let errors = dashboard
        .error
        .iter()
        .chain(dashboard.refresh_error.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let error = error_layout(&errors, Rect::new(0, 0, width, size.height), notice).0;
    let controls =
        input + notice + error + 3 + u16::from(dashboard.conversation.permission.is_some());
    let panel = if page == Page::Conversation {
        0
    } else {
        size.height.saturating_sub(controls + 1).min(14)
    };
    (controls + panel)
        .min(size.height.saturating_sub(1).max(1))
        .max(1)
}

fn draw(
    frame: &mut Frame<'_>,
    page: Page,
    dashboard: &Dashboard,
    animation: usize,
) -> Option<Position> {
    let area = frame
        .area()
        .inner(Margin::new(padding(frame.area().width), 0));
    let error_message = dashboard
        .error
        .iter()
        .chain(dashboard.refresh_error.iter())
        .cloned()
        .collect::<Vec<_>>()
        .join("\n");
    let notice_height = u16::from(dashboard.notice.is_some()) * 3;
    let (error_height, error_scroll) = error_layout(&error_message, area, notice_height);
    let input_height = if page == Page::Conversation {
        composer_height(dashboard, area.width, area.height + 7)
    } else {
        0
    };
    let [
        content,
        notice,
        error,
        permission,
        status,
        input,
        tabs,
        help,
    ] = Layout::vertical([
        Constraint::Min(0),
        Constraint::Length(notice_height),
        Constraint::Length(error_height),
        Constraint::Length(u16::from(dashboard.conversation.permission.is_some())),
        Constraint::Length(1),
        Constraint::Length(input_height),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    let mut cursor = None;
    match page {
        Page::Home => draw_home(frame, content, dashboard),
        Page::Agents => draw_agents(frame, content, dashboard),
        Page::Conversation => {
            cursor = draw_composer(frame, input, &dashboard.conversation);
        }
        Page::Sessions => draw_sessions(frame, content, dashboard),
        Page::Models => draw_models(frame, content, dashboard),
        Page::Requests => draw_requests(frame, content, dashboard),
        Page::Route => draw_route(frame, content, dashboard),
    }
    if let Some(message) = &dashboard.notice {
        frame.render_widget(
            Paragraph::new(message.as_str())
                .style(Style::default().fg(Color::Yellow))
                .wrap(Wrap { trim: true }),
            notice,
        );
    }
    if !error_message.is_empty() {
        frame.render_widget(
            Paragraph::new(error_message)
                .style(Style::default().fg(Color::Red))
                .block(Block::default().borders(Borders::TOP).title(" Error "))
                .wrap(Wrap { trim: true })
                .scroll((error_scroll, 0)),
            error,
        );
    }
    if let Some(prompt) = &dashboard.conversation.permission {
        frame.render_widget(Paragraph::new(prompt.render()), permission);
    }
    let busy =
        dashboard.conversation.status == "working" && dashboard.conversation.permission.is_none();
    let activity = if busy {
        format!("{} Thinking…", SPINNER[animation % SPINNER.len()])
    } else if dashboard.conversation.permission.is_some() {
        "Waiting for permission".to_string()
    } else {
        conversation_status(&dashboard.conversation)
    };
    frame.render_widget(
        Paragraph::new(format!("BitRouter · {} · {activity}", dashboard.target))
            .style(Style::default().fg(if busy { Color::Cyan } else { Color::DarkGray })),
        status,
    );
    frame.render_widget(
        Tabs::new([
            "Home",
            "Agents",
            "Conversation",
            "Sessions",
            "Models",
            "Requests",
            "Route",
        ])
        .select(page.index())
        .highlight_style(
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        )
        .divider("│"),
        tabs,
    );
    let hint = match page {
        Page::Agents => "↑/↓ select · Enter connect · Tab views · Ctrl-C quit",
        Page::Conversation => "Enter send · Shift+Enter/Ctrl+J newline · Tab views · Ctrl-D quit",
        Page::Route => "type model · Enter preview · Ctrl-U clear · Tab views · Ctrl-C quit",
        _ => "Tab views · r refresh · 1-7 jump · q/Esc/Ctrl-C quit",
    };
    frame.render_widget(
        Paragraph::new(hint).style(Style::default().fg(Color::DarkGray)),
        help,
    );
    cursor
}

/// Size a diagnostic pane from its wrapped rows while reserving the main view.
/// Long failures show their tail because adapter stderr usually puts the most
/// specific cause and remediation there.
fn error_layout(message: &str, area: ratatui::layout::Rect, notice_height: u16) -> (u16, u16) {
    if message.is_empty() {
        return (0, 0);
    }
    const HEADER_HEIGHT: u16 = 3;
    const FOOTER_HEIGHT: u16 = 1;
    const MIN_CONTENT_HEIGHT: u16 = 4;
    const MAX_ERROR_HEIGHT: u16 = 10;
    const ERROR_BORDER_HEIGHT: u16 = 1;

    let reserved = HEADER_HEIGHT
        .saturating_add(FOOTER_HEIGHT)
        .saturating_add(MIN_CONTENT_HEIGHT)
        .saturating_add(notice_height);
    let max_height = area.height.saturating_sub(reserved).min(MAX_ERROR_HEIGHT);
    if max_height == 0 {
        return (0, 0);
    }
    let wrapped_rows = Paragraph::new(message)
        .wrap(Wrap { trim: true })
        .line_count(area.width);
    let wrapped_rows = u16::try_from(wrapped_rows).unwrap_or(u16::MAX);
    let minimum = 3.min(max_height);
    let height = wrapped_rows
        .saturating_add(ERROR_BORDER_HEIGHT)
        .max(minimum)
        .min(max_height);
    let visible_rows = height.saturating_sub(ERROR_BORDER_HEIGHT);
    (height, wrapped_rows.saturating_sub(visible_rows))
}

fn draw_home(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
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
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Open Agents and press Enter to start an ACP conversation, or inspect operations views.",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(Block::default().borders(Borders::ALL).title(" Status "))
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_agents(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let rows = dashboard.agents.iter().enumerate().map(|(index, agent)| {
        let row = Row::new(vec![
            agent.id.clone(),
            if agent.native { "yes" } else { "—" }.to_string(),
            if agent.acp { "yes" } else { "—" }.to_string(),
            if agent.configured {
                "configured"
            } else {
                "catalog"
            }
            .to_string(),
            agent.description.clone(),
        ]);
        if index == dashboard.selected_agent {
            row.style(
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            )
        } else {
            row
        }
    });
    let table = Table::new(
        rows,
        [
            Constraint::Length(18),
            Constraint::Length(8),
            Constraint::Length(8),
            Constraint::Length(12),
            Constraint::Min(20),
        ],
    )
    .header(
        Row::new(["AGENT", "NATIVE", "ACP", "SOURCE", "DESCRIPTION"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(" Available agents "),
    );
    frame.render_widget(table, area);
}

fn draw_composer(
    frame: &mut Frame<'_>,
    area: Rect,
    conversation: &Conversation,
) -> Option<Position> {
    let block = Block::default()
        .borders(Borders::ALL)
        .border_style(Style::default().fg(Color::Cyan))
        .title(" Message ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.is_empty() {
        return None;
    }
    let layout = conversation.input.layout(inner.width);
    let offset = layout
        .cursor
        .0
        .saturating_sub(usize::from(inner.height).saturating_sub(1));
    let lines: Vec<Line<'static>> = layout
        .rows
        .iter()
        .skip(offset)
        .take(usize::from(inner.height))
        .cloned()
        .map(Line::from)
        .collect();
    frame.render_widget(Paragraph::new(lines), inner);
    if conversation.permission.is_none() {
        return Some(Position::new(
            inner.x + layout.cursor.1.min(inner.width.saturating_sub(1)),
            inner.y + u16::try_from(layout.cursor.0.saturating_sub(offset)).unwrap_or(0),
        ));
    }
    None
}

fn conversation_status(conversation: &Conversation) -> String {
    let Some(session) = conversation.native_session_id.as_deref() else {
        return "not connected · select an ACP-capable agent in Agents".to_string();
    };
    let status = if conversation.status.is_empty() {
        "idle"
    } else {
        conversation.status.as_str()
    };
    let route = conversation.route.as_deref().unwrap_or("direct");
    format!("{status} · session {session} · route {route}")
}

fn draw_sessions(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let mut lines = Vec::new();
    match dashboard.conversation.native_session_id.as_deref() {
        Some(session_id) => {
            lines.push(field(
                "agent",
                dashboard.conversation.agent.as_deref().unwrap_or("unknown"),
            ));
            lines.push(field("native id", session_id));
            if let Some(provider_id) = dashboard.conversation.provider_session_id.as_deref() {
                lines.push(field("provider id", provider_id));
            }
            lines.push(field(
                "lifecycle",
                dashboard.conversation.lifecycle.as_deref().unwrap_or("new"),
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Load replays history; resume does not. Close releases resources; delete is separate.",
            ));
        }
        None => lines.push(Line::from(
            "No active native session. Select an ACP-capable agent in Agents.",
        )),
    }
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(" Native sessions "),
            )
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

    fn rendered_text(terminal: &Terminal<TestBackend>) -> String {
        terminal
            .backend()
            .buffer()
            .content()
            .iter()
            .map(|cell| cell.symbol())
            .collect()
    }

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
            terminal.draw(|frame| {
                draw(frame, page, &dashboard, 0);
            })?;
        }
        Ok(())
    }

    #[test]
    fn multiline_dock_keeps_the_cursor_visible_at_small_sizes() -> io::Result<()> {
        let mut dashboard = Dashboard::default();
        dashboard
            .conversation
            .input
            .paste("first line\n你好 👩‍💻\nthird\nfourth\nfifth\nsixth\nseventh\neighth\nlast line");
        for (width, height) in [(110, 38), (40, 16), (20, 10), (8, 5), (1, 1)] {
            let height = dock_height(&dashboard, Page::Conversation, Size::new(width, height));
            let mut terminal = Terminal::new(TestBackend::new(width, height))?;
            let mut cursor = None;
            terminal.draw(|frame| {
                cursor = draw(frame, Page::Conversation, &dashboard, 0);
            })?;
            if let Some(cursor) = cursor {
                assert!(
                    cursor.x < width && cursor.y < height,
                    "{width}x{height}: {cursor:?}"
                );
                assert!(rendered_text(&terminal).contains("last line") || width < 20);
            }
        }
        Ok(())
    }

    #[test]
    fn thinking_animates_without_new_agent_chunks_and_stops_when_idle() -> io::Result<()> {
        let mut dashboard = Dashboard::default();
        dashboard.conversation.status = "working".to_string();
        let mut terminal = Terminal::new(TestBackend::new(100, 12))?;
        terminal.draw(|frame| {
            draw(frame, Page::Conversation, &dashboard, 0);
        })?;
        let first = rendered_text(&terminal);
        terminal.draw(|frame| {
            draw(frame, Page::Conversation, &dashboard, 1);
        })?;
        let second = rendered_text(&terminal);
        assert!(first.contains("⠋ Thinking…"));
        assert!(second.contains("⠙ Thinking…"));
        dashboard.conversation.status = "idle".to_string();
        terminal.draw(|frame| {
            draw(frame, Page::Conversation, &dashboard, 2);
        })?;
        assert!(!rendered_text(&terminal).contains("Thinking…"));
        Ok(())
    }

    #[test]
    fn transcript_keeps_both_gutters_without_dropping_wide_graphemes() {
        let content = "你好世界这是一个终端测试 👩‍💻 hello world";
        for width in [9, 13, 20, 31, 80] {
            let rows = padded_transcript(&[Line::from(content)], width);
            let text = rows.iter().map(crate::plain::text).collect::<String>();
            assert_eq!(text.replace(' ', ""), content.replace(' ', ""));
            for row in rows {
                assert!(crate::plain::text(&row).starts_with("  "));
                assert!(row.width() <= usize::from(width - 2));
            }
        }
    }

    #[test]
    fn error_layout_grows_to_a_cap_and_scrolls_to_the_tail() {
        let area = ratatui::layout::Rect::new(0, 0, 120, 40);
        assert_eq!(error_layout("short failure", area, 0), (3, 0));

        let long = format!(
            "{}Missing optional dependency @openai/codex-darwin-arm64",
            "launch context and transport details ".repeat(80)
        );
        let (height, scroll) = error_layout(&long, area, 0);
        assert_eq!(height, 10);
        assert!(scroll > 0);

        let (with_notice, _) = error_layout(&long, ratatui::layout::Rect::new(0, 0, 120, 16), 3);
        assert_eq!(with_notice, 5);
    }

    #[test]
    fn long_error_renders_actionable_tail_beside_persistent_notice() -> io::Result<()> {
        let backend = TestBackend::new(120, 40);
        let mut terminal = Terminal::new(backend)?;
        let dashboard = Dashboard {
            target: "local".to_string(),
            notice: Some("routing fallback remains visible".to_string()),
            error: Some(format!(
                "{}Missing optional dependency @openai/codex-darwin-arm64",
                "connection failed while launching adapter context ".repeat(80)
            )),
            ..Dashboard::default()
        };

        terminal.draw(|frame| {
            draw(frame, Page::Agents, &dashboard, 0);
        })?;
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("routing fallback remains visible"));
        assert!(rendered.contains("Missing optional dependency"));
        assert!(rendered.contains("@openai/codex-darwin-arm64"));
        Ok(())
    }

    #[test]
    fn reducer_preserves_multiline_draft_while_agents_change() {
        let mut dashboard = Dashboard {
            agents: vec![
                AgentLine {
                    id: "claude".to_string(),
                    native: true,
                    acp: true,
                    configured: false,
                    description: "Claude".to_string(),
                },
                AgentLine {
                    id: "codex".to_string(),
                    native: true,
                    acp: true,
                    configured: false,
                    description: "Codex".to_string(),
                },
            ],
            ..Dashboard::default()
        };
        let _ = step(
            &mut dashboard,
            Action::Key(KeyEvent::new(
                crossterm::event::KeyCode::Char('d'),
                crossterm::event::KeyModifiers::NONE,
            )),
        );
        let _ = step(&mut dashboard, Action::Paste("raft\ntext".to_string()));
        let _ = step(&mut dashboard, Action::SelectNextAgent);
        assert_eq!(dashboard.conversation.input.line(), "draft\ntext");
        assert_eq!(dashboard.selected_agent, 1);
        assert_eq!(
            step(&mut dashboard, Action::ActivateSelectedAgent),
            Some(Effect::StartAgent("codex".to_string()))
        );
    }

    #[test]
    fn reducer_submits_one_nonempty_prompt_and_clears_the_composer() {
        let mut dashboard = Dashboard::default();
        let _ = step(&mut dashboard, Action::Paste("review this".to_string()));
        assert_eq!(
            step(
                &mut dashboard,
                Action::Key(KeyEvent::new(
                    crossterm::event::KeyCode::Enter,
                    crossterm::event::KeyModifiers::NONE
                ))
            ),
            Some(Effect::Prompt("review this".to_string()))
        );
        assert!(dashboard.conversation.input.line().is_empty());
        assert_eq!(
            step(
                &mut dashboard,
                Action::Key(KeyEvent::new(
                    crossterm::event::KeyCode::Enter,
                    crossterm::event::KeyModifiers::NONE
                ))
            ),
            None
        );
    }

    #[test]
    fn disconnected_conversation_has_no_route_or_empty_status_prefix() {
        let status = conversation_status(&Conversation::default());
        assert_eq!(
            status,
            "not connected · select an ACP-capable agent in Agents"
        );
        assert!(!status.contains("route direct"));
        assert!(!status.starts_with('·'));
    }

    #[test]
    fn connected_conversation_identifies_a_direct_session() {
        let status = conversation_status(&Conversation {
            native_session_id: Some("session-1".to_string()),
            ..Conversation::default()
        });
        assert_eq!(status, "idle · session session-1 · route direct");
    }

    #[test]
    fn page_navigation_cycles_through_one_unified_shell() {
        let mut page = Page::Home;
        for expected in [
            Page::Agents,
            Page::Conversation,
            Page::Sessions,
            Page::Models,
            Page::Requests,
            Page::Route,
            Page::Home,
        ] {
            page = page.next();
            assert_eq!(page, expected);
        }
    }
}
