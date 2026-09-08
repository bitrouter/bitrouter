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
    Providers,
    Telemetry,
    Policy,
    Reload,
}

impl Page {
    const ALL: [Self; 11] = [
        Self::Home,
        Self::Agents,
        Self::Conversation,
        Self::Sessions,
        Self::Models,
        Self::Requests,
        Self::Route,
        Self::Providers,
        Self::Telemetry,
        Self::Policy,
        Self::Reload,
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
            Self::Providers => 7,
            Self::Telemetry => 8,
            Self::Policy => 9,
            Self::Reload => 10,
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
    pub provider_inventory: Vec<ProviderLine>,
    pub telemetry: Option<TelemetryLine>,
    pub policy: Option<PolicyLine>,
    pub policy_detail: Option<PolicyDetailLine>,
    /// The requested source is explicit even before the first policy report
    /// arrives, so toggling never silently falls back to a different source.
    pub policy_view: PolicyViewSelection,
    pub selected_policy: usize,
    pub policy_scroll: u16,
    pub reload: Option<ReloadLine>,
    pub reload_operation: Option<ReloadOperationLine>,
    /// Each passive read retains its last good data when a later refresh
    /// fails, together with its own time and diagnostic.
    pub status_refresh: RefreshState,
    pub models_refresh: RefreshState,
    pub requests_refresh: RefreshState,
    pub providers_refresh: RefreshState,
    pub agents_refresh: RefreshState,
    pub telemetry_refresh: RefreshState,
    pub policy_refresh: RefreshState,
    pub policy_detail_refresh: RefreshState,
    pub reload_refresh: RefreshState,
    /// Cached discovery grants. `false` disables the corresponding control;
    /// `None` means discovery itself has not answered yet.
    pub control_read_authorized: Option<bool>,
    pub control_reload_authorized: Option<bool>,
    pub authority_refresh: RefreshState,
    pub reload_action: RefreshState,
    /// An operator explicitly initiated a reload and its bounded remote poll
    /// or local socket call has not finished yet.
    pub reload_in_flight: bool,
    pub route_input: String,
    pub route: Option<RouteLine>,
    /// Action failure retained until that action later succeeds.
    pub error: Option<String>,
    /// Connection/snapshot failure cleared by the next successful refresh.
    pub refresh_error: Option<String>,
    /// Non-fatal action diagnostic, such as an ACP routing fallback.
    pub notice: Option<String>,
    pub agents: Vec<AgentLine>,
    /// Remote administration exposes the catalog but never agent launch
    /// controls or ACP sessions.
    pub can_launch_agents: bool,
    pub selected_agent: usize,
    pub conversation: Conversation,
}

#[derive(Debug, Clone, Default)]
pub struct RefreshState {
    pub updated_at: Option<String>,
    pub error: Option<String>,
}

#[derive(Debug, Clone)]
pub struct AgentLine {
    pub id: String,
    pub native: bool,
    pub acp: bool,
    pub configured: bool,
    pub description: String,
}

#[derive(Debug, Clone)]
pub struct ProviderLine {
    pub id: String,
    pub models: usize,
    pub active: bool,
}

#[derive(Debug, Clone)]
pub struct TelemetryLine {
    pub daemon_reachable: bool,
    pub compiled_in: bool,
    pub exporter_wired: bool,
    pub sampler: Option<String>,
    pub metrics_enabled: bool,
    pub header_count: usize,
    pub resource_attribute_count: usize,
    pub api_key_count: usize,
    pub api_key_cap: usize,
    pub user_id_count: usize,
    pub user_id_cap: usize,
    pub active_spans: usize,
}

#[derive(Debug, Clone)]
pub struct PolicyLine {
    pub view: String,
    pub availability: String,
    pub digest: Option<String>,
    pub mode: String,
    pub policies: Vec<String>,
    pub bindings: Vec<(String, String)>,
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum PolicyViewSelection {
    #[default]
    Active,
    Disk,
}

impl PolicyViewSelection {
    pub fn toggle(self) -> Self {
        match self {
            Self::Active => Self::Disk,
            Self::Disk => Self::Active,
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Self::Active => "active",
            Self::Disk => "disk",
        }
    }
}

#[derive(Debug, Clone)]
pub struct PolicyDetailLine {
    pub name: String,
    pub tiers: Vec<PolicyTierLine>,
    pub routes: Vec<PolicyRouteLine>,
    pub default_tier: Option<String>,
    pub tool_use_tier: Option<String>,
    pub tool_safe_tiers: Vec<String>,
    pub certificates: Vec<PolicyCertificateLine>,
}

#[derive(Debug, Clone)]
pub struct PolicyTierLine {
    pub tier: String,
    pub target: String,
}

#[derive(Debug, Clone)]
pub struct PolicyRouteLine {
    pub route: String,
    pub tier: String,
}

#[derive(Debug, Clone)]
pub struct PolicyCertificateLine {
    pub name: String,
    pub selected_tier: String,
    pub evidence_digest: String,
    pub compiler_config_digest: String,
    pub evaluator_config_digest: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReloadLine {
    pub server_instance_id: String,
    pub generation: u64,
    pub running: bool,
    pub running_generation: Option<u64>,
    pub consistency: String,
    pub last_outcome: Option<String>,
    pub participants: Vec<ReloadParticipantLine>,
    pub restart_required_fields: Vec<String>,
    pub mixed_state_history: usize,
}

#[derive(Debug, Clone)]
pub struct ReloadParticipantLine {
    pub participant: String,
    pub outcome: String,
    pub detail: Option<String>,
}

#[derive(Debug, Clone)]
pub struct ReloadOperationLine {
    pub request_id: String,
    pub server_instance_id: String,
    pub generation_before: u64,
    pub status: String,
    pub lookup: String,
    pub completed_at_unix_ms: Option<i64>,
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
    let available = size.height.saturating_sub(controls + 1);
    let panel = match page {
        Page::Conversation => 0,
        // Typed policy detail and retained reload receipts need more space
        // than the compact inventory views, while keeping controls docked.
        Page::Policy | Page::Reload => available,
        _ => available.min(14),
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
        Page::Providers => draw_providers(frame, content, dashboard),
        Page::Telemetry => draw_telemetry(frame, content, dashboard),
        Page::Policy => draw_policy(frame, content, dashboard),
        Page::Reload => draw_reload(frame, content, dashboard),
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
            "Providers",
            "Telemetry",
            "Policy",
            "Reload",
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
        Page::Agents if dashboard.can_launch_agents => {
            "↑/↓ select · Enter connect · Tab views · Ctrl-C quit"
        }
        Page::Agents => "remote catalog is read-only · Tab views · Ctrl-C quit",
        Page::Conversation => "Enter send · Shift+Enter/Ctrl+J newline · Tab views · Ctrl-D quit",
        Page::Route => "type model · Enter preview · Ctrl-U clear · Tab views · Ctrl-C quit",
        Page::Policy => {
            "↑/↓ select · Enter detail · v active/disk · PgUp/PgDn scroll · r refresh · Tab views"
        }
        Page::Reload => "Enter/Ctrl-R reload · r refresh · Tab views · q/Esc/Ctrl-C quit",
        _ => "Tab views · r refresh · 1-0 jump · q/Esc/Ctrl-C quit",
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
    lines.push(Line::from(if dashboard.can_launch_agents {
        "Open Agents and press Enter to start an ACP conversation, or inspect operations views."
    } else {
        "Inspect the remote agent catalog and operations views from this read-only dashboard."
    }));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(panel_title("Status", &dashboard.status_refresh)),
            )
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
            .title(panel_title("Agent catalog", &dashboard.agents_refresh)),
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
    .block(Block::default().borders(Borders::ALL).title(panel_title(
        &format!("Models · {}", dashboard.models.len()),
        &dashboard.models_refresh,
    )));
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
    .block(Block::default().borders(Borders::ALL).title(panel_title(
        &format!("Recent requests · {}", dashboard.requests.len()),
        &dashboard.requests_refresh,
    )));
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

fn draw_providers(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let rows = dashboard.provider_inventory.iter().map(|provider| {
        Row::new(vec![
            provider.id.clone(),
            provider.models.to_string(),
            (if provider.active { "yes" } else { "no" }).to_string(),
        ])
    });
    let table = Table::new(
        rows,
        [
            Constraint::Percentage(50),
            Constraint::Length(12),
            Constraint::Length(10),
        ],
    )
    .header(
        Row::new(["PROVIDER", "MODELS", "ACTIVE"])
            .style(Style::default().add_modifier(Modifier::BOLD)),
    )
    .block(
        Block::default()
            .borders(Borders::ALL)
            .title(panel_title("Providers", &dashboard.providers_refresh)),
    );
    frame.render_widget(table, area);
}

fn draw_telemetry(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let lines = match &dashboard.telemetry {
        Some(telemetry) => {
            let mut lines = vec![
                field(
                    "daemon",
                    if telemetry.daemon_reachable {
                        "reachable"
                    } else {
                        "stopped"
                    },
                ),
                field("compiled", if telemetry.compiled_in { "yes" } else { "no" }),
                field(
                    "wired",
                    if telemetry.exporter_wired {
                        "yes"
                    } else {
                        "no"
                    },
                ),
                field(
                    "metrics",
                    if telemetry.metrics_enabled {
                        "on"
                    } else {
                        "off"
                    },
                ),
                field("headers", &telemetry.header_count.to_string()),
                field("res-attrs", &telemetry.resource_attribute_count.to_string()),
                field(
                    "api keys",
                    &format!("{} / {}", telemetry.api_key_count, telemetry.api_key_cap),
                ),
                field(
                    "users",
                    &format!("{} / {}", telemetry.user_id_count, telemetry.user_id_cap),
                ),
                field("in-flight", &telemetry.active_spans.to_string()),
            ];
            if let Some(sampler) = &telemetry.sampler {
                lines.insert(3, field("sampler", sampler));
            }
            lines
        }
        None => vec![Line::from("Telemetry has not returned a snapshot yet.")],
    };
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(panel_title("Telemetry", &dashboard.telemetry_refresh)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn draw_policy(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let mut lines = match &dashboard.policy {
        Some(policy) => {
            let mut lines = vec![
                field("view", &policy.view),
                field("availability", &policy.availability),
                field("mode", &policy.mode),
            ];
            if let Some(digest) = &policy.digest {
                lines.push(field("digest", digest));
            }
            if !policy.policies.is_empty() {
                lines.push(Line::from(""));
                lines.push(Line::from("Policies (Up/Down, Enter for typed detail):"));
                for (index, name) in policy.policies.iter().enumerate() {
                    let marker = if index == dashboard.selected_policy {
                        ">"
                    } else {
                        " "
                    };
                    lines.push(Line::from(format!(" {marker} {name}")));
                }
            }
            for (preset, policy) in &policy.bindings {
                lines.push(Line::from(format!("  @{preset} -> {policy}")));
            }
            lines
        }
        None => vec![Line::from(format!(
            "{} policy has not returned a snapshot yet.",
            dashboard.policy_view.label()
        ))],
    };
    if let Some(detail) = &dashboard.policy_detail {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("Detail: {}", detail.name)));
        if let Some(tier) = &detail.default_tier {
            lines.push(field("default tier", tier));
        }
        if let Some(tier) = &detail.tool_use_tier {
            lines.push(field("tool tier", tier));
        }
        if !detail.tool_safe_tiers.is_empty() {
            lines.push(field("tool safe", &detail.tool_safe_tiers.join(", ")));
        }
        if !detail.tiers.is_empty() {
            lines.push(Line::from("  Tiers:"));
            for tier in &detail.tiers {
                lines.push(Line::from(format!("    {} -> {}", tier.tier, tier.target)));
            }
        }
        if !detail.routes.is_empty() {
            lines.push(Line::from("  Routes:"));
            for route in &detail.routes {
                lines.push(Line::from(format!("    {} -> {}", route.route, route.tier)));
            }
        }
        if !detail.certificates.is_empty() {
            lines.push(Line::from("  Certificates:"));
            for certificate in &detail.certificates {
                lines.push(Line::from(format!(
                    "    {}: tier={}",
                    certificate.name, certificate.selected_tier
                )));
                lines.push(Line::from(format!(
                    "      evidence={} compiler={}",
                    certificate.evidence_digest, certificate.compiler_config_digest
                )));
                if let Some(evaluator) = &certificate.evaluator_config_digest {
                    lines.push(Line::from(format!("      evaluator={evaluator}")));
                }
            }
        }
    }
    if let Some(error) = &dashboard.policy_detail_refresh.error {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("Policy detail: {error}")));
    } else if let Some(updated) = &dashboard.policy_detail_refresh.updated_at {
        lines.push(Line::from(format!("Policy detail updated {updated}")));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(format!(
        "Source: {} · press v to switch active/disk · PgUp/PgDn scroll detail",
        dashboard.policy_view.label()
    )));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(panel_title("Policy", &dashboard.policy_refresh)),
            )
            .wrap(Wrap { trim: false })
            .scroll((dashboard.policy_scroll, 0)),
        area,
    );
}

fn draw_reload(frame: &mut Frame<'_>, area: ratatui::layout::Rect, dashboard: &Dashboard) {
    let mut lines = match &dashboard.reload {
        Some(reload) => {
            let mut lines = vec![
                field("instance", &reload.server_instance_id),
                field("generation", &reload.generation.to_string()),
                field("running", if reload.running { "yes" } else { "no" }),
                field("consistency", &reload.consistency),
                field(
                    "read scope",
                    authority_text(dashboard.control_read_authorized),
                ),
                field(
                    "reload scope",
                    authority_text(dashboard.control_reload_authorized),
                ),
            ];
            if let Some(generation) = reload.running_generation {
                lines.push(field("running-gen", &generation.to_string()));
            }
            if let Some(outcome) = &reload.last_outcome {
                lines.push(field("last outcome", outcome));
            }
            if !reload.restart_required_fields.is_empty() {
                lines.push(field("restart", &reload.restart_required_fields.join(", ")));
            }
            if reload.mixed_state_history > 0 {
                lines.push(field(
                    "mixed history",
                    &reload.mixed_state_history.to_string(),
                ));
            }
            for participant in &reload.participants {
                let detail = participant
                    .detail
                    .as_ref()
                    .map_or_else(String::new, |detail| format!(" · {detail}"));
                lines.push(Line::from(format!(
                    "  {}: {}{detail}",
                    participant.participant, participant.outcome
                )));
            }
            lines
        }
        None => vec![
            Line::from("Reload state has not returned a snapshot yet."),
            field(
                "read scope",
                authority_text(dashboard.control_read_authorized),
            ),
            field(
                "reload scope",
                authority_text(dashboard.control_reload_authorized),
            ),
        ],
    };
    if let Some(operation) = &dashboard.reload_operation {
        lines.push(Line::from(""));
        lines.push(field("request", &operation.request_id));
        lines.push(field("op status", &operation.status));
        lines.push(field("op instance", &operation.server_instance_id));
        lines.push(field(
            "op generation",
            &operation.generation_before.to_string(),
        ));
        if let Some(completed) = operation.completed_at_unix_ms {
            lines.push(field("op completed", &completed.to_string()));
        }
        lines.push(field("lookup", &operation.lookup));
    }
    if dashboard.reload_in_flight {
        lines.push(Line::from(
            "Reload action: submitting or polling the retained operation…",
        ));
    } else if let Some(error) = &dashboard.reload_action.error {
        lines.push(Line::from(""));
        lines.push(Line::from(format!("Reload action: {error}")));
    } else if let Some(updated) = &dashboard.reload_action.updated_at {
        lines.push(Line::from(format!("Reload action updated {updated}")));
    }
    if let Some(error) = &dashboard.authority_refresh.error {
        lines.push(Line::from(format!("Capability discovery: {error}")));
    } else if let Some(updated) = &dashboard.authority_refresh.updated_at {
        lines.push(Line::from(format!(
            "Capability discovery updated {updated}"
        )));
    }
    if dashboard.control_reload_authorized == Some(false) {
        lines.push(Line::from(
            "Reload action is unavailable for this credential; refresh after a scope change.",
        ));
    }
    lines.push(Line::from(""));
    lines.push(Line::from(
        "Press Enter or Ctrl-R to reload. Inspect failed, mixed, or unknown results before retrying.",
    ));
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::ALL)
                    .title(panel_title("Reload", &dashboard.reload_refresh)),
            )
            .wrap(Wrap { trim: false }),
        area,
    );
}

fn authority_text(authorized: Option<bool>) -> &'static str {
    match authorized {
        Some(true) => "granted",
        Some(false) => "not granted",
        None => "unknown",
    }
}

fn panel_title(name: &str, state: &RefreshState) -> String {
    match (&state.updated_at, &state.error) {
        (Some(updated), Some(error)) => format!(" {name} · stale since {updated} · {error} "),
        (None, Some(error)) => format!(" {name} · unavailable · {error} "),
        (Some(updated), None) => format!(" {name} · updated {updated} "),
        (None, None) => format!(" {name} "),
    }
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
    fn policy_page_renders_a_selected_typed_detail_and_source() -> io::Result<()> {
        let dashboard = Dashboard {
            target: "remote:workstation".to_string(),
            policy_view: PolicyViewSelection::Disk,
            policy: Some(PolicyLine {
                view: "disk".to_string(),
                availability: "available".to_string(),
                digest: Some("policy-digest".to_string()),
                mode: "frozen".to_string(),
                policies: vec!["default".to_string()],
                bindings: Vec::new(),
            }),
            policy_detail: Some(PolicyDetailLine {
                name: "default".to_string(),
                tiers: vec![PolicyTierLine {
                    tier: "fast".to_string(),
                    target: "openai/gpt-5".to_string(),
                }],
                routes: vec![PolicyRouteLine {
                    route: "code".to_string(),
                    tier: "fast".to_string(),
                }],
                default_tier: Some("fast".to_string()),
                tool_use_tier: None,
                tool_safe_tiers: Vec::new(),
                certificates: vec![PolicyCertificateLine {
                    name: "proof".to_string(),
                    selected_tier: "fast".to_string(),
                    evidence_digest: "evidence".to_string(),
                    compiler_config_digest: "compiler".to_string(),
                    evaluator_config_digest: Some("evaluator".to_string()),
                }],
            }),
            ..Dashboard::default()
        };

        let height = dock_height(&dashboard, Page::Policy, Size::new(160, 60));
        let mut terminal = Terminal::new(TestBackend::new(160, height))?;
        terminal.draw(|frame| {
            draw(frame, Page::Policy, &dashboard, 0);
        })?;
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("Detail: default"));
        assert!(rendered.contains("fast -> openai/gpt-5"));
        assert!(rendered.contains("code -> fast"));
        assert!(rendered.contains("proof: tier=fast"));
        assert!(rendered.contains("Source: disk"));
        Ok(())
    }

    #[test]
    fn reload_page_marks_a_missing_reload_scope_unavailable() -> io::Result<()> {
        let backend = TestBackend::new(160, 40);
        let mut terminal = Terminal::new(backend)?;
        let dashboard = Dashboard {
            target: "remote:reader".to_string(),
            control_read_authorized: Some(true),
            control_reload_authorized: Some(false),
            ..Dashboard::default()
        };

        terminal.draw(|frame| {
            draw(frame, Page::Reload, &dashboard, 0);
        })?;
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("reload scope"));
        assert!(rendered.contains("not granted"));
        assert!(rendered.contains("Reload action is unavailable"));
        Ok(())
    }

    #[test]
    fn reload_dock_keeps_the_retained_receipt_visible() -> io::Result<()> {
        let dashboard = Dashboard {
            reload: Some(ReloadLine {
                server_instance_id: "boot".to_string(),
                generation: 2,
                running: false,
                running_generation: None,
                consistency: "mixed".to_string(),
                last_outcome: Some("partially_applied".to_string()),
                participants: ["config", "routing", "providers", "policy", "telemetry"]
                    .into_iter()
                    .map(|participant| ReloadParticipantLine {
                        participant: participant.to_string(),
                        outcome: "applied".to_string(),
                        detail: None,
                    })
                    .collect(),
                restart_required_fields: Vec::new(),
                mixed_state_history: 1,
            }),
            reload_operation: Some(ReloadOperationLine {
                request_id: "retained-request".to_string(),
                server_instance_id: "boot".to_string(),
                generation_before: 1,
                status: "partially_applied".to_string(),
                lookup: "bitrouter operations show retained-request".to_string(),
                completed_at_unix_ms: Some(1000),
            }),
            ..Dashboard::default()
        };
        let height = dock_height(&dashboard, Page::Reload, Size::new(160, 40));
        let mut terminal = Terminal::new(TestBackend::new(160, height))?;
        terminal.draw(|frame| {
            draw(frame, Page::Reload, &dashboard, 0);
        })?;
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("partially_applied"));
        assert!(rendered.contains("bitrouter operations show retained-request"));
        assert!(rendered.contains("Inspect failed, mixed, or unknown results before retrying"));
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
            Page::Providers,
            Page::Telemetry,
            Page::Policy,
            Page::Reload,
            Page::Home,
        ] {
            page = page.next();
            assert_eq!(page, expected);
        }
    }
}
