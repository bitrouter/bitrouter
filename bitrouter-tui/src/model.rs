use std::collections::{HashMap, VecDeque};
use std::time::Instant;

use bitrouter_providers::acp::types::{PermissionRequest, PermissionResponse, ToolCallStatus};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

// ── Agent color palette ─────────────────────────────────────────────────

const AGENT_PALETTE: &[Color] = &[
    Color::Green,
    Color::Cyan,
    Color::Yellow,
    Color::Magenta,
    Color::Blue,
    Color::LightRed,
    Color::LightGreen,
    Color::LightCyan,
];

/// Pick a color from the palette by index.
pub fn agent_color(index: usize) -> Color {
    AGENT_PALETTE[index % AGENT_PALETTE.len()]
}

// ── Renderable trait ───────────────────────────────────────────────────

/// Context passed to `Renderable::render_lines` for resolving agent colors.
pub struct RenderContext {
    pub agent_colors: HashMap<String, Color>,
}

/// Trait for entry types that know how to render themselves into styled lines.
pub trait Renderable {
    fn render_lines(&self, width: u16, collapsed: bool, ctx: &RenderContext) -> Vec<Line<'static>>;
}

// ── Agent ───────────────────────────────────────────────────────────────

/// An agent harness that can be connected via ACP.
#[derive(Debug, Clone)]
pub struct Agent {
    pub name: String,
    pub config: Option<bitrouter_config::AgentConfig>,
    pub status: AgentStatus,
    pub session_id: Option<String>,
    pub color: Color,
}

/// Connection lifecycle of an agent.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentStatus {
    /// Discovered on PATH, not connected.
    Idle,
    /// Thread spawned, awaiting `AgentConnected`.
    Connecting,
    /// Session active, ready for prompts.
    Connected,
    /// Processing a prompt (awaiting `PromptDone`).
    Busy,
    /// Connection failed or crashed.
    Error(String),
}

// ── Entry types ────────────────────────────────────────────────────────

/// Monotonic entry identifier.
pub type EntryId = u64;

/// A single entry in the unified scrollback.
pub struct ActivityEntry {
    pub id: EntryId,
    pub kind: EntryKind,
    pub timestamp: Instant,
    /// Whether this entry is visually collapsed.
    pub collapsed: bool,
}

/// The payload of a scrollback entry.
pub enum EntryKind {
    UserPrompt(UserPrompt),
    AgentResponse(AgentResponse),
    ToolCall(ToolCallEntry),
    Thinking(ThinkingEntry),
    Permission(PermissionEntry),
    System(SystemNotice),
}

impl Renderable for EntryKind {
    fn render_lines(&self, width: u16, collapsed: bool, ctx: &RenderContext) -> Vec<Line<'static>> {
        match self {
            Self::UserPrompt(e) => e.render_lines(width, collapsed, ctx),
            Self::AgentResponse(e) => e.render_lines(width, collapsed, ctx),
            Self::ToolCall(e) => e.render_lines(width, collapsed, ctx),
            Self::Thinking(e) => e.render_lines(width, collapsed, ctx),
            Self::Permission(e) => e.render_lines(width, collapsed, ctx),
            Self::System(e) => e.render_lines(width, collapsed, ctx),
        }
    }
}

// ── Gutter constants ───────────────────────────────────────────────────

const GUTTER: &str = "⎿  ";
const PROMPT_PREFIX: &str = "› ";

// ── UserPrompt ─────────────────────────────────────────────────────────

pub struct UserPrompt {
    pub text: String,
    pub targets: Vec<String>,
}

impl Renderable for UserPrompt {
    fn render_lines(
        &self,
        width: u16,
        _collapsed: bool,
        _ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        let mut lines = Vec::new();
        let text_width = (width as usize).saturating_sub(PROMPT_PREFIX.len());

        let target_suffix = if self.targets.is_empty() {
            String::new()
        } else {
            format!(" → {}", self.targets.join(", "))
        };

        for (i, raw_line) in self.text.lines().enumerate() {
            for (j, segment) in wrap_line(raw_line, text_width).into_iter().enumerate() {
                let prefix = if i == 0 && j == 0 {
                    Span::styled(
                        PROMPT_PREFIX.to_string(),
                        Style::default()
                            .fg(Color::Cyan)
                            .add_modifier(Modifier::BOLD),
                    )
                } else {
                    Span::raw("  ")
                };

                let mut spans = vec![prefix, Span::raw(segment)];

                // Add target suffix on the first line only.
                if i == 0 && j == 0 && !target_suffix.is_empty() {
                    spans.push(Span::styled(
                        target_suffix.clone(),
                        Style::default().fg(Color::DarkGray),
                    ));
                }

                lines.push(Line::from(spans));
            }
        }

        // Empty line after the prompt block.
        lines.push(Line::raw(""));
        lines
    }
}

// ── AgentResponse ──────────────────────────────────────────────────────

pub struct AgentResponse {
    pub agent_id: String,
    pub blocks: Vec<ContentBlock>,
    pub is_streaming: bool,
}

impl Renderable for AgentResponse {
    fn render_lines(&self, width: u16, collapsed: bool, ctx: &RenderContext) -> Vec<Line<'static>> {
        let color = ctx
            .agent_colors
            .get(&self.agent_id)
            .copied()
            .unwrap_or(Color::White);
        let text_width = (width as usize).saturating_sub(GUTTER.len());
        let mut lines = Vec::new();

        // Agent name header.
        lines.push(Line::from(Span::styled(
            format!("  {}", self.agent_id),
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        )));

        if collapsed {
            lines.push(Line::from(vec![
                gutter_span(color),
                Span::styled("[collapsed]", Style::default().fg(Color::DarkGray)),
            ]));
        } else {
            for block in &self.blocks {
                match block {
                    ContentBlock::Text(text) => {
                        for raw_line in text.lines() {
                            for segment in wrap_line(raw_line, text_width) {
                                lines
                                    .push(Line::from(vec![gutter_span(color), Span::raw(segment)]));
                            }
                        }
                    }
                    ContentBlock::Other(desc) => {
                        lines.push(Line::from(vec![
                            gutter_span(color),
                            Span::styled(desc.clone(), Style::default().fg(Color::DarkGray)),
                        ]));
                    }
                }
            }
            if self.is_streaming {
                lines.push(Line::from(vec![
                    gutter_span(color),
                    Span::styled("▌", Style::default().fg(Color::Cyan)),
                ]));
            }
        }

        lines.push(Line::raw(""));
        lines
    }
}

// ── ToolCallEntry ──────────────────────────────────────────────────────

pub struct ToolCallEntry {
    pub agent_id: String,
    pub tool_call_id: String,
    pub title: String,
    pub status: ToolCallStatus,
}

impl Renderable for ToolCallEntry {
    fn render_lines(
        &self,
        _width: u16,
        _collapsed: bool,
        ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        let color = ctx
            .agent_colors
            .get(&self.agent_id)
            .copied()
            .unwrap_or(Color::White);
        let (icon, icon_color) = tool_status_icon(&self.status);

        vec![Line::from(vec![
            gutter_span(color),
            Span::styled(
                format!("{icon} {}", self.title),
                Style::default().fg(icon_color),
            ),
        ])]
    }
}

// ── ThinkingEntry ──────────────────────────────────────────────────────

pub struct ThinkingEntry {
    pub agent_id: String,
    pub text: String,
    pub is_streaming: bool,
}

impl Renderable for ThinkingEntry {
    fn render_lines(&self, width: u16, collapsed: bool, ctx: &RenderContext) -> Vec<Line<'static>> {
        let color = ctx
            .agent_colors
            .get(&self.agent_id)
            .copied()
            .unwrap_or(Color::White);
        let text_width = (width as usize).saturating_sub(GUTTER.len());
        let mut lines = Vec::new();

        if collapsed {
            lines.push(Line::from(vec![
                gutter_span(color),
                Span::styled(
                    "◌ thinking...",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]));
        } else {
            lines.push(Line::from(vec![
                gutter_span(color),
                Span::styled(
                    "◌ Thinking...",
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::DIM),
                ),
            ]));
            for raw_line in self.text.lines() {
                for segment in wrap_line(raw_line, text_width) {
                    lines.push(Line::from(vec![
                        gutter_span(color),
                        Span::styled(segment, Style::default().fg(Color::DarkGray)),
                    ]));
                }
            }
            if self.is_streaming {
                lines.push(Line::from(vec![
                    gutter_span(color),
                    Span::styled("▌", Style::default().fg(Color::DarkGray)),
                ]));
            }
        }

        lines
    }
}

// ── PermissionEntry ────────────────────────────────────────────────────

pub struct PermissionEntry {
    pub agent_id: String,
    pub request: Box<PermissionRequest>,
    pub response_tx: Option<tokio::sync::oneshot::Sender<PermissionResponse>>,
    pub resolved: bool,
}

impl Renderable for PermissionEntry {
    fn render_lines(
        &self,
        _width: u16,
        _collapsed: bool,
        ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        let color = ctx
            .agent_colors
            .get(&self.agent_id)
            .copied()
            .unwrap_or(Color::White);
        let mut lines = Vec::new();

        let title = if self.request.title.is_empty() {
            "unknown tool".to_string()
        } else {
            self.request.title.clone()
        };

        if self.resolved {
            lines.push(Line::from(vec![
                gutter_span(color),
                Span::styled(format!("✓ {title}"), Style::default().fg(Color::DarkGray)),
            ]));
        } else {
            lines.push(Line::from(vec![
                gutter_span(color),
                Span::styled(
                    format!("⚠ {title}"),
                    Style::default()
                        .fg(Color::Yellow)
                        .add_modifier(Modifier::BOLD),
                ),
            ]));
            lines.push(Line::from(vec![
                Span::raw("    "),
                Span::styled(
                    "(y)es / (n)o / (a)lways",
                    Style::default().fg(Color::Yellow),
                ),
            ]));
        }

        lines
    }
}

// ── SystemNotice ───────────────────────────────────────────────────────

pub struct SystemNotice {
    pub text: String,
}

impl Renderable for SystemNotice {
    fn render_lines(
        &self,
        _width: u16,
        _collapsed: bool,
        _ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        vec![Line::from(Span::styled(
            format!("  {}", self.text),
            Style::default().fg(Color::DarkGray),
        ))]
    }
}

// ── BackgroundAgentSummary (rendered inline, not stored as entry) ──────

pub struct BackgroundAgentSummary {
    pub agent_id: String,
    pub tool_call_count: usize,
    pub elapsed_secs: u64,
}

impl Renderable for BackgroundAgentSummary {
    fn render_lines(
        &self,
        width: u16,
        _collapsed: bool,
        ctx: &RenderContext,
    ) -> Vec<Line<'static>> {
        let color = ctx
            .agent_colors
            .get(&self.agent_id)
            .copied()
            .unwrap_or(Color::White);

        let info = format!(
            "◎ working · {} tool calls · {}s",
            self.tool_call_count, self.elapsed_secs
        );

        // Fill with dots between agent name and info.
        let prefix = format!("  ┄ {} ", self.agent_id);
        let suffix_len = info.len() + 1; // +1 for trailing space
        let fill_len = (width as usize)
            .saturating_sub(prefix.len())
            .saturating_sub(suffix_len);
        let dots: String = std::iter::repeat_n('·', fill_len).collect();

        vec![
            Line::from(vec![
                Span::styled(prefix, Style::default().fg(color)),
                Span::styled(format!("{dots} "), Style::default().fg(Color::DarkGray)),
                Span::styled(info, Style::default().fg(Color::DarkGray)),
            ]),
            Line::raw(""),
        ]
    }
}

// ── Content block ──────────────────────────────────────────────────────

/// A content block inside an agent message.
#[derive(Debug, Clone)]
pub enum ContentBlock {
    Text(String),
    Other(String),
}

// ── Scrollback state ───────────────────────────────────────────────────

/// Multi-agent focus: which agent is expanded vs. collapsed to a summary.
pub struct AgentFocus {
    /// The agent whose output is fully expanded. `None` = no concurrent agents.
    pub focused: Option<String>,
    /// All agents currently streaming or just finished for the current prompt.
    pub active: Vec<String>,
}

impl AgentFocus {
    pub fn new() -> Self {
        Self {
            focused: None,
            active: Vec::new(),
        }
    }

    /// Cycle focus to the next active agent. Returns the newly focused agent id.
    pub fn cycle(&mut self) -> Option<&str> {
        if self.active.len() <= 1 {
            return self.focused.as_deref();
        }
        let current_idx = self
            .focused
            .as_ref()
            .and_then(|f| self.active.iter().position(|a| a == f))
            .unwrap_or(0);
        let next_idx = (current_idx + 1) % self.active.len();
        self.focused = self.active.get(next_idx).cloned();
        self.focused.as_deref()
    }

    /// Returns true if this agent is a background agent (active but not focused).
    pub fn is_background(&self, agent_id: &str) -> bool {
        self.active.len() > 1
            && self.focused.as_deref() != Some(agent_id)
            && self.active.iter().any(|a| a == agent_id)
    }
}

/// State of the unified scrollback.
pub struct ScrollbackState {
    pub entries: Vec<ActivityEntry>,
    pub scroll_offset: usize,
    /// Recomputed each render pass for scroll clamping.
    pub total_rendered_lines: usize,
    /// Counter for generating unique `EntryId`s.
    pub next_entry_id: u64,
    /// Per-agent streaming cursor: maps agent_id → EntryId of the entry
    /// currently being streamed into.
    pub streaming_entry: HashMap<String, EntryId>,
    /// Multi-agent focus state.
    pub agent_focus: AgentFocus,
    /// Whether the viewport is pinned to the bottom (auto-scroll).
    pub follow: bool,
}

impl ScrollbackState {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
            scroll_offset: 0,
            total_rendered_lines: 0,
            next_entry_id: 0,
            streaming_entry: HashMap::new(),
            agent_focus: AgentFocus::new(),
            follow: true,
        }
    }

    /// Allocate a new `EntryId`.
    pub fn next_id(&mut self) -> EntryId {
        let id = self.next_entry_id;
        self.next_entry_id += 1;
        id
    }

    /// Find the index of an entry by its `EntryId`.
    pub fn index_of(&self, id: EntryId) -> Option<usize> {
        self.entries.iter().position(|e| e.id == id)
    }
}

// ── Inline input ───────────────────────────────────────────────────────

/// Lightweight inline input replacing ratatui-textarea.
pub struct InlineInput {
    pub lines: Vec<String>,
    /// Cursor position: (row, col).
    pub cursor: (usize, usize),
}

impl InlineInput {
    pub fn new() -> Self {
        Self {
            lines: vec![String::new()],
            cursor: (0, 0),
        }
    }

    pub fn insert_char(&mut self, ch: char) {
        let (row, col) = self.cursor;
        if let Some(line) = self.lines.get_mut(row) {
            let byte_idx = char_to_byte_idx(line, col);
            line.insert(byte_idx, ch);
            self.cursor.1 = col + 1;
        }
    }

    pub fn backspace(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            if let Some(line) = self.lines.get_mut(row) {
                let byte_idx = char_to_byte_idx(line, col - 1);
                let end_idx = char_to_byte_idx(line, col);
                line.drain(byte_idx..end_idx);
                self.cursor.1 = col - 1;
            }
        } else if row > 0 {
            // Merge with previous line.
            let current = self.lines.remove(row);
            let prev_len = self.lines[row - 1].chars().count();
            self.lines[row - 1].push_str(&current);
            self.cursor = (row - 1, prev_len);
        }
    }

    pub fn delete_char(&mut self) {
        let (row, col) = self.cursor;
        if let Some(line) = self.lines.get_mut(row) {
            let char_count = line.chars().count();
            if col < char_count {
                let byte_idx = char_to_byte_idx(line, col);
                let end_idx = char_to_byte_idx(line, col + 1);
                line.drain(byte_idx..end_idx);
            } else if row + 1 < self.lines.len() {
                // Merge next line into current.
                let next = self.lines.remove(row + 1);
                self.lines[row].push_str(&next);
            }
        }
    }

    pub fn newline(&mut self) {
        let (row, col) = self.cursor;
        if let Some(line) = self.lines.get_mut(row) {
            let byte_idx = char_to_byte_idx(line, col);
            let rest = line[byte_idx..].to_string();
            line.truncate(byte_idx);
            self.lines.insert(row + 1, rest);
            self.cursor = (row + 1, 0);
        }
    }

    pub fn move_left(&mut self) {
        let (row, col) = self.cursor;
        if col > 0 {
            self.cursor.1 = col - 1;
        } else if row > 0 {
            self.cursor.0 = row - 1;
            self.cursor.1 = self.lines[row - 1].chars().count();
        }
    }

    pub fn move_right(&mut self) {
        let (row, col) = self.cursor;
        if let Some(line) = self.lines.get(row) {
            let char_count = line.chars().count();
            if col < char_count {
                self.cursor.1 = col + 1;
            } else if row + 1 < self.lines.len() {
                self.cursor = (row + 1, 0);
            }
        }
    }

    pub fn move_up(&mut self) {
        if self.cursor.0 > 0 {
            self.cursor.0 -= 1;
            let line_len = self.lines[self.cursor.0].chars().count();
            self.cursor.1 = self.cursor.1.min(line_len);
        }
    }

    pub fn move_down(&mut self) {
        if self.cursor.0 + 1 < self.lines.len() {
            self.cursor.0 += 1;
            let line_len = self.lines[self.cursor.0].chars().count();
            self.cursor.1 = self.cursor.1.min(line_len);
        }
    }

    pub fn home(&mut self) {
        self.cursor.1 = 0;
    }

    pub fn end(&mut self) {
        if let Some(line) = self.lines.get(self.cursor.0) {
            self.cursor.1 = line.chars().count();
        }
    }

    /// Get the full text (all lines joined with newlines).
    pub fn text(&self) -> String {
        self.lines.join("\n")
    }

    /// Reset to empty state.
    pub fn clear(&mut self) {
        self.lines = vec![String::new()];
        self.cursor = (0, 0);
    }

    pub fn is_empty(&self) -> bool {
        self.lines.len() == 1 && self.lines[0].is_empty()
    }

    /// Delete n characters before the cursor (for autocomplete acceptance).
    pub fn delete_before(&mut self, n: usize) {
        for _ in 0..n {
            self.backspace();
        }
    }

    /// Insert a string at the cursor.
    pub fn insert_str(&mut self, s: &str) {
        for ch in s.chars() {
            self.insert_char(ch);
        }
    }
}

/// Convert a char index to a byte index within a string.
fn char_to_byte_idx(s: &str, char_idx: usize) -> usize {
    s.char_indices()
        .nth(char_idx)
        .map_or(s.len(), |(byte_idx, _)| byte_idx)
}

// ── Input target ───────────────────────────────────────────────────────

/// Resolved target(s) for the current input message.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum InputTarget {
    /// No @-mention — route to default agent.
    Default,
    /// One or more specific agents.
    Specific(Vec<String>),
    /// Broadcast to all connected agents.
    All,
}

/// Autocomplete state for @-mentions in the input bar.
pub struct AutocompleteState {
    /// The prefix typed after `@` (e.g. `"cl"` for `@cl`).
    pub prefix: String,
    /// Matching agent names (filtered).
    pub candidates: Vec<String>,
    /// Index of the highlighted candidate.
    pub selected: usize,
}

// ── Modals ──────────────────────────────────────────────────────────────

/// At most one modal overlay is open at a time.
pub enum Modal {
    AgentManager(AgentManagerState),
    Observability(ObservabilityState),
    CommandPalette(CommandPaletteState),
    Help,
}

pub struct AgentManagerState {
    pub selected: usize,
}

pub struct ObservabilityState {
    pub scroll_offset: usize,
}

/// A recorded event for the observability panel.
pub struct ObsEvent {
    pub agent_id: String,
    pub kind: ObsEventKind,
    pub timestamp: Instant,
}

pub enum ObsEventKind {
    Connected,
    Disconnected,
    PromptSent,
    PromptDone,
    ToolCall { title: String },
    Error { message: String },
}

/// Capped observability event log.
pub struct ObsLog {
    pub events: VecDeque<ObsEvent>,
}

impl ObsLog {
    const MAX_EVENTS: usize = 1000;

    pub fn new() -> Self {
        Self {
            events: VecDeque::new(),
        }
    }

    pub fn push(&mut self, event: ObsEvent) {
        if self.events.len() >= Self::MAX_EVENTS {
            self.events.pop_front();
        }
        self.events.push_back(event);
    }
}

// ── Command palette ─────────────────────────────────────────────────────

pub struct CommandPaletteState {
    pub query: String,
    pub all_commands: Vec<PaletteCommand>,
    /// Indices into `all_commands` that match the current query.
    pub filtered: Vec<usize>,
    pub selected: usize,
}

pub struct PaletteCommand {
    pub label: String,
    pub action: CommandAction,
}

#[derive(Debug, Clone)]
pub enum CommandAction {
    ConnectAgent(String),
    DisconnectAgent(String),
    SetDefaultAgent(String),
    ToggleObservability,
    ClearConversation,
    ShowHelp,
}

// ── Helpers ─────────────────────────────────────────────────────────────

fn gutter_span(color: Color) -> Span<'static> {
    Span::styled(GUTTER.to_string(), Style::default().fg(color))
}

fn tool_status_icon(status: &ToolCallStatus) -> (&'static str, Color) {
    match status {
        ToolCallStatus::Pending => ("○", Color::DarkGray),
        ToolCallStatus::InProgress => ("⠋", Color::Yellow),
        ToolCallStatus::Completed => ("✓", Color::Green),
        ToolCallStatus::Failed => ("✗", Color::Red),
    }
}

/// Simple word-wrap at the given width.
pub fn wrap_line(line: &str, width: usize) -> Vec<String> {
    if width == 0 || line.is_empty() {
        return vec![line.to_string()];
    }
    let mut result = Vec::new();
    let mut current = String::new();
    for word in line.split_whitespace() {
        if current.is_empty() {
            current.push_str(word);
        } else if current.len() + 1 + word.len() <= width {
            current.push(' ');
            current.push_str(word);
        } else {
            result.push(std::mem::take(&mut current));
            current.push_str(word);
        }
    }
    if !current.is_empty() {
        result.push(current);
    }
    if result.is_empty() {
        result.push(String::new());
    }
    result
}
