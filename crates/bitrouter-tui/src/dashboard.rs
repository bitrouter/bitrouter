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
    pub input: String,
    pub scroll: usize,
    pub journal: crate::journal::Journal,
    pub permission: Option<crate::permission::Prompt>,
    /// Confirmed application evidence state; distinct from native lifecycle.
    pub task: Option<TaskView>,
}

/// Plain task display data supplied by the application's ACP consumer.
#[derive(Debug, Clone)]
pub struct TaskView {
    pub task_id: Option<String>,
    pub attempt_id: Option<String>,
    pub status: String,
    pub next_prompt: Option<String>,
    pub hint: String,
}

/// Pure Code-shell state transition. Async launch/prompt effects stay in the
/// application driver; this reducer owns selection, drafts, and scrolling.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    SelectPreviousAgent,
    SelectNextAgent,
    ActivateSelectedAgent,
    Type(char),
    Paste(String),
    Backspace,
    SubmitPrompt,
    ScrollUp,
    ScrollDown,
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
        Action::Type(character) => {
            dashboard.conversation.input.push(character);
            None
        }
        Action::Paste(text) => {
            dashboard
                .conversation
                .input
                .push_str(&text.replace(['\n', '\r'], " "));
            None
        }
        Action::Backspace => {
            dashboard.conversation.input.pop();
            None
        }
        Action::SubmitPrompt => {
            let prompt = std::mem::take(&mut dashboard.conversation.input);
            (!prompt.trim().is_empty()).then_some(Effect::Prompt(prompt))
        }
        Action::ScrollUp => {
            dashboard.conversation.scroll = dashboard.conversation.scroll.saturating_add(10);
            None
        }
        Action::ScrollDown => {
            dashboard.conversation.scroll = dashboard.conversation.scroll.saturating_sub(10);
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
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    page: Page,
    conversation_cache: crate::writer::Cache,
    conversation_registry: crate::render::Registry,
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
            page: Page::Home,
            conversation_cache: crate::writer::Cache::default(),
            conversation_registry: crate::render::Registry::default(),
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
        let cache = &mut self.conversation_cache;
        let registry = &self.conversation_registry;
        self.terminal
            .draw(|frame| {
                let conversation = (page == Page::Conversation).then(|| {
                    cache.document(
                        &dashboard.conversation.journal,
                        registry,
                        ratatui::layout::Size::new(
                            frame.area().width.max(1),
                            frame.area().height.max(1),
                        ),
                        &[],
                    )
                });
                draw(frame, page, dashboard, conversation.as_deref());
            })
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

fn draw(
    frame: &mut Frame<'_>,
    page: Page,
    dashboard: &Dashboard,
    conversation: Option<&[Line<'static>]>,
) {
    let area = frame.area();
    let error_messages = dashboard
        .error
        .iter()
        .chain(dashboard.refresh_error.iter())
        .cloned()
        .collect::<Vec<_>>();
    let error_message = error_messages.join("\n");
    let notice_height = u16::from(dashboard.notice.is_some()) * 3;
    let (error_height, error_scroll) = error_layout(&error_message, area, notice_height);
    let [header, content, notice, error, footer] = Layout::vertical([
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(notice_height),
        Constraint::Length(error_height),
        Constraint::Length(1),
    ])
    .areas(area);

    let titles = [
        "Home",
        "Agents",
        "Conversation",
        "Sessions",
        "Models",
        "Requests",
        "Route",
    ]
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
        Page::Home => draw_home(frame, content, dashboard),
        Page::Agents => draw_agents(frame, content, dashboard),
        Page::Conversation => {
            draw_conversation(frame, content, dashboard, conversation.unwrap_or_default())
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
                .block(Block::default().borders(Borders::TOP).title(" Notice "))
                .wrap(Wrap { trim: true }),
            notice,
        );
    }

    if !error_messages.is_empty() {
        frame.render_widget(
            Paragraph::new(error_message)
                .style(Style::default().fg(Color::Red))
                .block(Block::default().borders(Borders::TOP).title(" Error "))
                .wrap(Wrap { trim: true })
                .scroll((error_scroll, 0)),
            error,
        );
    }

    let help = match page {
        Page::Agents => "↑/↓ select · Enter connect · Tab pages · Esc/Ctrl-C quit",
        Page::Conversation => {
            "type prompt · Enter send · PgUp/PgDn scroll · Tab views · Ctrl-D quit"
        }
        Page::Route => "Tab pages · type model · Enter preview · Ctrl-U clear · Esc/Ctrl-C quit",
        _ => "Tab pages · r refresh · 1-7 jump · q/Esc/Ctrl-C quit",
    };
    frame.render_widget(
        Paragraph::new(help).style(Style::default().fg(Color::DarkGray)),
        footer,
    );
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

fn draw_conversation(
    frame: &mut Frame<'_>,
    area: ratatui::layout::Rect,
    dashboard: &Dashboard,
    document: &[Line<'static>],
) {
    let [transcript, status, input] = Layout::vertical([
        Constraint::Min(4),
        Constraint::Length(if dashboard.conversation.task.is_some() {
            5
        } else {
            2
        }),
        Constraint::Length(3),
    ])
    .areas(area);
    let visible = usize::from(transcript.height.saturating_sub(2));
    let end = document.len().saturating_sub(dashboard.conversation.scroll);
    let start = end.saturating_sub(visible);
    frame.render_widget(
        Paragraph::new(document.get(start..end).unwrap_or_default().to_vec())
            .block(Block::default().borders(Borders::ALL).title(" Transcript "))
            .wrap(Wrap { trim: false }),
        transcript,
    );
    let mut status_lines = vec![Line::from(conversation_status(&dashboard.conversation))];
    if let Some(task) = &dashboard.conversation.task {
        let short = |value: &Option<String>| {
            value
                .as_deref()
                .unwrap_or("pending")
                .chars()
                .take(8)
                .collect::<String>()
        };
        status_lines.push(Line::from(format!(
            "Task {} · attempt {}",
            short(&task.task_id),
            short(&task.attempt_id)
        )));
        status_lines.push(Line::from(task.status.clone()));
        if let Some(next) = &task.next_prompt {
            status_lines.push(Line::from(next.clone()));
        }
        status_lines.push(Line::from(task.hint.clone()));
    }
    frame.render_widget(
        Paragraph::new(status_lines).style(Style::default().fg(Color::DarkGray)),
        status,
    );
    let title =
        dashboard
            .conversation
            .permission
            .as_ref()
            .map_or(" Message ".to_string(), |permission| {
                format!(
                    " Permission: {} · choose 1-9, Esc denies ",
                    permission.title()
                )
            });
    frame.render_widget(
        Paragraph::new(dashboard.conversation.input.as_str())
            .block(Block::default().borders(Borders::ALL).title(title)),
        input,
    );
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
            lines.push(field("ACP session", session_id));
            if let Some(provider_id) = dashboard.conversation.provider_session_id.as_deref() {
                lines.push(field("reported agent id", provider_id));
            }
            lines.push(field(
                "lifecycle",
                dashboard.conversation.lifecycle.as_deref().unwrap_or("new"),
            ));
            lines.push(Line::from(""));
            lines.push(Line::from(
                "Load replays history; resume does not. Close releases resources; delete is separate.",
            ));
            if let Some(task) = &dashboard.conversation.task {
                lines.push(Line::from(""));
                lines.push(field(
                    "task",
                    task.task_id
                        .as_deref()
                        .unwrap_or("starts with the first message"),
                ));
                lines.push(field(
                    "attempt",
                    task.attempt_id.as_deref().unwrap_or("pending"),
                ));
                lines.push(Line::from(task.status.clone()));
                if let Some(next) = &task.next_prompt {
                    lines.push(Line::from(next.clone()));
                }
                lines.push(Line::from(
                    "New task and retry apply to your next message and keep the native context.",
                ));
                lines.push(Line::from("Task controls are available in Conversation."));
            }
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
                    .title(" Session and task "),
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
            terminal.draw(|frame| draw(frame, page, &dashboard, None))?;
        }
        Ok(())
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

        terminal.draw(|frame| draw(frame, Page::Agents, &dashboard, None))?;
        let rendered = rendered_text(&terminal);
        assert!(rendered.contains("routing fallback remains visible"));
        assert!(rendered.contains("Missing optional dependency"));
        assert!(rendered.contains("@openai/codex-darwin-arm64"));
        Ok(())
    }

    #[test]
    fn reducer_preserves_draft_while_agents_and_scroll_change() {
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
        let _ = step(&mut dashboard, Action::Type('d'));
        let _ = step(&mut dashboard, Action::Paste("raft\ntext".to_string()));
        let _ = step(&mut dashboard, Action::SelectNextAgent);
        let _ = step(&mut dashboard, Action::ScrollUp);
        assert_eq!(dashboard.conversation.input, "draft text");
        assert_eq!(dashboard.selected_agent, 1);
        assert_eq!(dashboard.conversation.scroll, 10);
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
            step(&mut dashboard, Action::SubmitPrompt),
            Some(Effect::Prompt("review this".to_string()))
        );
        assert!(dashboard.conversation.input.is_empty());
        assert_eq!(step(&mut dashboard, Action::SubmitPrompt), None);
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
    fn conversation_renders_task_selection_without_claiming_evaluation_complete() -> io::Result<()>
    {
        let mut dashboard = Dashboard::default();
        dashboard.conversation.native_session_id = Some("native-1".into());
        dashboard.conversation.input = "my next instruction".into();
        dashboard.conversation.task = Some(TaskView {
            task_id: Some("task-first".into()),
            attempt_id: Some("attempt-first".into()),
            status: "Reconciling task evidence; evaluation pending".into(),
            next_prompt: Some("Next message starts another attempt of this task.".into()),
            hint: "F2 New task · F3 Retry task · F4 Refresh task".into(),
        });
        let mut terminal = Terminal::new(TestBackend::new(100, 24))?;
        terminal.draw(|frame| draw(frame, Page::Conversation, &dashboard, None))?;
        let text = rendered_text(&terminal);
        assert!(text.contains("evaluation pending"));
        assert!(text.contains("Next message starts another attempt"));
        assert!(text.contains("F2 New task"));
        assert!(text.contains("my next instruction"));
        terminal.draw(|frame| draw(frame, Page::Sessions, &dashboard, None))?;
        let text = rendered_text(&terminal);
        assert!(text.contains("task-first"));
        assert!(text.contains("attempt-first"));
        assert!(text.contains("native-1"));
        Ok(())
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
