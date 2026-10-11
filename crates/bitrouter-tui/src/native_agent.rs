//! Native-scrollback BRO conversation. Execution, history and queue authority
//! remain in the server; this module retains only the client's presentation.

use std::io::{self, IsTerminal};

use crossterm::event::Event;
use ratatui::Terminal;
use ratatui::backend::{CrosstermBackend, TestBackend};
use ratatui::layout::{Constraint, Layout, Margin, Position, Rect, Size};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::agents_menu::{AgentsMenu, MenuEntry};
use crate::editor::{Edit, Editor};
use crate::wrap::wrap;
use crate::writer::{Writer, buffer_lines};

pub enum NativeEntryKind {
    User,
    Assistant,
    Detail,
    Tool { status: ToolStatus, detail: String },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ToolStatus {
    Running,
    Succeeded,
    Failed,
    Denied,
    Unknown,
}

pub struct NativeLive {
    pub item_id: Option<String>,
    pub text: String,
    pub truncated: bool,
}

pub struct NativeEntry {
    pub kind: NativeEntryKind,
    pub id: String,
    pub text: String,
}

#[derive(Default)]
pub struct NativeState {
    pub model: String,
    pub workspace: String,
    pub thread_id: Option<String>,
    pub turn_id: Option<String>,
    pub status: String,
    pub verification: String,
    pub entries: Vec<NativeEntry>,
    pub live: Option<NativeLive>,
    pub pending_input_id: Option<String>,
    pub pending_input_detail: Option<String>,
    pub queued: usize,
    pub notice: Option<String>,
    pub editor: Editor,
    pub model_editor: Option<Editor>,
    pub menu: AgentsMenu,
    pub inventory: Vec<MenuEntry>,
    pub inventory_help: String,
    pub viewport: Option<Size>,
}

impl NativeState {
    pub fn push(&mut self, line: impl Into<String>) {
        self.notice = Some(line.into());
    }

    /// Replace a committed entity while preserving its first-seen position.
    pub fn upsert(&mut self, id: String, text: String, kind: NativeEntryKind) {
        if self
            .live
            .as_ref()
            .is_some_and(|live| live.item_id.as_deref() == Some(&id))
        {
            self.live = None;
        }
        if let Some(entry) = self.entries.iter_mut().find(|entry| entry.id == id) {
            entry.text = text;
            entry.kind = kind;
        } else {
            self.entries.push(NativeEntry { id, text, kind });
        }
    }

    pub fn edit(&mut self, event: &Event) -> Edit {
        edit(&mut self.editor, event)
    }

    pub fn input_ready(&self) -> bool {
        self.viewport
            .is_none_or(|size| size.width >= 40 && size.height >= 16)
    }

    pub fn can_open_agents(&self) -> bool {
        self.editor.is_empty()
            && self.model_editor.as_ref().is_none_or(Editor::is_empty)
            && self.pending_input_id.is_none()
    }
}

pub fn edit(editor: &mut Editor, event: &Event) -> Edit {
    match event {
        Event::Key(key) => editor.apply(*key),
        Event::Paste(text) => {
            editor.paste(text);
            Edit::Changed
        }
        _ => Edit::Ignored,
    }
}

pub struct NativeView {
    writer: Writer<CrosstermBackend<io::Stdout>>,
    document: Vec<Line<'static>>,
    thread_id: Option<String>,
}

impl NativeView {
    pub fn open() -> io::Result<Self> {
        if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
            return Err(io::Error::other(
                "bro code requires an interactive terminal",
            ));
        }
        crate::lifecycle::install_panic_restore();
        crate::lifecycle::enter_raw()?;
        if let Err(error) = crate::lifecycle::enable_session_keys() {
            crate::lifecycle::restore();
            return Err(error);
        }
        match Writer::new(CrosstermBackend::new(io::stdout())).and_then(|mut writer| {
            writer.claim_full_height()?;
            Ok(writer)
        }) {
            Ok(writer) => Ok(Self {
                writer,
                document: Vec::new(),
                thread_id: None,
            }),
            Err(error) => {
                crate::lifecycle::restore();
                Err(error)
            }
        }
    }

    pub fn invalidate(&mut self) {
        self.writer.invalidate();
    }

    pub fn suspend(&mut self) -> io::Result<()> {
        self.writer.finish()?;
        crate::lifecycle::restore();
        Ok(())
    }

    pub fn resume(&mut self) -> io::Result<()> {
        crate::lifecycle::enter_raw()?;
        crate::lifecycle::enable_session_keys()?;
        self.writer.invalidate();
        Ok(())
    }

    pub fn draw(&mut self, state: &mut NativeState) -> io::Result<()> {
        if self.thread_id != state.thread_id {
            // Creating the first Thread keeps the entry header in place.
            if self.thread_id.is_some() {
                self.writer.new_document()?;
                self.writer.claim_full_height()?;
            }
            self.thread_id = state.thread_id.clone();
            self.document.clear();
        }
        let size = self.writer.size();
        let resized = state.viewport != Some(size);
        state.viewport = Some(size);
        if !state.menu.is_open() || self.document.is_empty() || resized {
            self.document = document(state, size.width);
        }
        let menu_height = size.height.saturating_mul(2) / 5;
        let editor = state.model_editor.as_ref().unwrap_or(&state.editor);
        let editor_rows = composer_rows(editor, size.width.saturating_sub(4).max(1));
        let height = if state.menu.is_open() {
            menu_height.max(6)
        } else {
            u16::try_from(editor_rows.0.len())
                .unwrap_or(u16::MAX)
                .min(6)
                .saturating_add(
                    5 + u16::from(state.notice.is_some())
                        + u16::from(state.pending_input_detail.is_some()),
                )
        }
        .min(size.height.max(1));
        let mut dock = Terminal::new(TestBackend::new(size.width.max(1), height.max(1)))?;
        let frame = dock.draw(|frame| render_dock(frame, state))?;
        let footer = buffer_lines(frame.buffer);
        self.writer.docked_frame(&self.document, &footer)?;
        let cursor = if state.menu.is_open() {
            None
        } else {
            let (_, position) = editor_rows;
            let visible = height
                .saturating_sub(
                    5 + u16::from(state.notice.is_some())
                        + u16::from(state.pending_input_detail.is_some()),
                )
                .max(1);
            let first = position.y.saturating_sub(visible.saturating_sub(1));
            Some(Position::new(
                position.x.saturating_add(2),
                size.height
                    .saturating_sub(height)
                    .saturating_add(
                        2 + u16::from(state.notice.is_some())
                            + u16::from(state.pending_input_detail.is_some()),
                    )
                    .saturating_add(position.y.saturating_sub(first)),
            ))
        };
        self.writer.cursor(cursor)
    }
}

impl Drop for NativeView {
    fn drop(&mut self) {
        let _ = self.writer.finish();
        crate::lifecycle::restore();
    }
}

fn safe(text: &str) -> String {
    text.chars()
        .map(|ch| {
            if ch.is_control() && ch != '\n' {
                ' '
            } else {
                ch
            }
        })
        .collect()
}

/// Clip a status row by terminal cells, keeping Unicode graphemes intact.
fn compact(text: &str, width: u16) -> String {
    let text = safe(text).split_whitespace().collect::<Vec<_>>().join(" ");
    if text.width() <= usize::from(width) {
        return text;
    }
    let mut result = String::new();
    let mut cells = 0;
    for grapheme in text.graphemes(true) {
        if cells + grapheme.width() > usize::from(width.saturating_sub(1)) {
            break;
        }
        result.push_str(grapheme);
        cells += grapheme.width();
    }
    if width > 0 {
        result.push('…');
    }
    result
}

fn document(state: &NativeState, width: u16) -> Vec<Line<'static>> {
    let inner = width.saturating_sub(4).max(1);
    let muted = Style::default().fg(Color::DarkGray);
    let mut lines = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(
                ">_ BRO",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::styled(format!("  BitRouter v{}", env!("CARGO_PKG_VERSION")), muted),
        ]),
        Line::from("Your coding workspace, ready."),
        Line::styled(compact(&state.workspace, inner), muted),
        Line::default(),
    ];
    let mut previous_tool = false;
    for entry in &state.entries {
        let text = safe(&entry.text);
        if text.is_empty() {
            continue;
        }
        let tool = matches!(entry.kind, NativeEntryKind::Tool { .. });
        if !tool || !previous_tool {
            lines.push(Line::default());
        }
        match &entry.kind {
            NativeEntryKind::Assistant => assistant_lines(&mut lines, &text, inner, false),
            NativeEntryKind::User => {
                lines.push(Line::styled(
                    "› You",
                    Style::default()
                        .fg(Color::Cyan)
                        .add_modifier(Modifier::BOLD),
                ));
                lines.extend(text.lines().map(|line| Line::from(line.to_string())));
            }
            NativeEntryKind::Detail => lines.extend(
                text.lines()
                    .map(|line| Line::styled(line.to_string(), muted)),
            ),
            NativeEntryKind::Tool { status, detail } => {
                let (label, color) = match status {
                    ToolStatus::Running => ("◌ running", Color::Yellow),
                    ToolStatus::Succeeded => ("✓ done", Color::Green),
                    ToolStatus::Failed => ("× failed", Color::Red),
                    ToolStatus::Denied => ("− denied", Color::Yellow),
                    ToolStatus::Unknown => ("? unknown", Color::Yellow),
                };
                let body = if detail.is_empty() {
                    text
                } else {
                    format!("{text} · {detail}")
                };
                lines.push(Line::from(vec![
                    Span::styled(format!("{label}  "), Style::default().fg(color)),
                    Span::raw(compact(
                        &body,
                        inner.saturating_sub(label.width() as u16 + 2),
                    )),
                ]));
            }
        }
        previous_tool = tool;
    }
    if let Some(live) = &state.live {
        lines.push(Line::default());
        assistant_lines(&mut lines, &safe(&live.text), inner, live.truncated);
    }
    lines.push(Line::default());
    // Wrap before adding margins so every continuation row keeps its padding.
    lines
        .into_iter()
        .flat_map(|line| wrap(&line, inner))
        .map(|mut line| {
            line.spans.insert(0, Span::raw("  "));
            line
        })
        .collect()
}

fn assistant_lines(lines: &mut Vec<Line<'static>>, text: &str, width: u16, truncated: bool) {
    lines.push(Line::styled(
        "● BRO",
        Style::default().add_modifier(Modifier::BOLD),
    ));
    if truncated {
        lines.push(Line::styled(
            "Earlier streaming output omitted",
            Style::default().fg(Color::DarkGray),
        ));
    }
    lines.extend(crate::render::markdown::render(text, width));
}

fn composer_rows(editor: &Editor, width: u16) -> (Vec<Line<'static>>, Position) {
    let layout = crate::composer::layout(
        editor.text(),
        editor.cursor_byte(),
        width,
        "›",
        Style::default().fg(Color::Cyan),
    );
    (
        layout.rows,
        Position::new(
            layout.cursor_column,
            u16::try_from(layout.cursor_row).unwrap_or(u16::MAX),
        ),
    )
}

fn render_dock(frame: &mut ratatui::Frame<'_>, state: &NativeState) {
    let area = frame.area();
    if !state.input_ready() {
        frame.render_widget(Paragraph::new("Resize to at least 40×16. Draft and server queue retained. Esc returns; Ctrl-D detaches."), area);
        return;
    }
    if state.menu.is_open() {
        state.menu.render_entries(
            frame,
            area,
            &state.inventory,
            usize::from(state.pending_input_id.is_some()),
            true,
        );
        if !state.inventory_help.is_empty() && state.menu.is_listing() {
            frame.render_widget(
                Paragraph::new(state.inventory_help.clone())
                    .style(Style::default().fg(Color::DarkGray)),
                Rect::new(area.x, area.bottom().saturating_sub(1), area.width, 1),
            );
        }
        return;
    }
    let area = area.inner(Margin::new(2, 0));
    let [
        notice,
        approval,
        label,
        top_padding,
        composer,
        bottom_padding,
        context,
        help,
    ] = Layout::vertical([
        Constraint::Length(u16::from(state.notice.is_some())),
        Constraint::Length(u16::from(state.pending_input_detail.is_some())),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Min(1),
        Constraint::Length(1),
        Constraint::Length(1),
        Constraint::Length(1),
    ])
    .areas(area);
    if let Some(text) = &state.notice {
        frame.render_widget(Paragraph::new(safe(text)), notice);
    }
    if let Some(text) = &state.pending_input_detail {
        frame.render_widget(
            Paragraph::new(safe(text).replace('\n', " ")).style(Style::default().fg(Color::Yellow)),
            approval,
        );
    }
    let editor = state.model_editor.as_ref().unwrap_or(&state.editor);
    let caption = if state.model_editor.is_some() {
        "Enter a routed model ID"
    } else {
        "─ Message ─"
    };
    frame.render_widget(
        Paragraph::new(caption).style(Style::default().fg(Color::DarkGray)),
        label,
    );
    let input_style = Style::default().bg(Color::Indexed(236)).fg(Color::White);
    frame.render_widget(Paragraph::new("").style(input_style), top_padding);
    frame.render_widget(Paragraph::new("").style(input_style), bottom_padding);
    let (rows, position) = composer_rows(editor, composer.width.max(1));
    let first =
        usize::from(position.y).saturating_sub(usize::from(composer.height.saturating_sub(1)));
    let lines = rows.into_iter().skip(first).collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(if editor.is_empty() && state.model_editor.is_none() {
            vec![Line::from("› Ask BitRouter to do anything")]
        } else {
            lines
        })
        .style(input_style),
        composer,
    );
    let hint = if state.can_open_agents() {
        " · ← agents"
    } else {
        ""
    };
    let id = state.thread_id.as_deref().unwrap_or("new");
    let mut facts = format!(
        "{} · thread: {} · status: {} · queue: {}",
        state.model,
        id.get(..8).unwrap_or(id),
        state.status,
        state.queued
    );
    if context.width >= 110 {
        facts.push_str(&format!(" · check: {}", state.verification));
    }
    let available = usize::from(context.width).saturating_sub(hint.chars().count());
    let mut width = 0;
    let facts = facts
        .chars()
        .take_while(|ch| {
            width += unicode_width::UnicodeWidthChar::width(*ch).unwrap_or(0);
            width <= available
        })
        .collect::<String>();
    frame.render_widget(
        Paragraph::new(format!("{facts}{hint}")).style(Style::default().fg(Color::DarkGray)),
        context,
    );
    frame.render_widget(
        Paragraph::new("Enter send/queue · Ctrl-Enter steer · Ctrl-R resume · Ctrl-D detach")
            .style(Style::default().fg(Color::DarkGray)),
        help,
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn committed_items_replace_in_place_and_preserve_order() {
        let mut state = NativeState::default();
        state.upsert("tool-a".into(), "started".into(), NativeEntryKind::Detail);
        state.upsert(
            "answer-b".into(),
            "answer".into(),
            NativeEntryKind::Assistant,
        );
        state.upsert("tool-a".into(), "completed".into(), NativeEntryKind::Detail);
        assert_eq!(state.entries.len(), 2);
        assert_eq!(state.entries[0].text, "completed");
        assert_eq!(state.entries[1].id, "answer-b");
    }

    #[test]
    fn tool_rows_are_single_line_with_unicode_and_control_safe_summaries() {
        let mut state = NativeState::default();
        for (id, status) in [("one", ToolStatus::Running), ("two", ToolStatus::Failed)] {
            state.upsert(
                id.into(),
                format!("shell {}\nsecond line\x1b[2J", "你好👩‍💻 ".repeat(30)),
                NativeEntryKind::Tool {
                    status,
                    detail: "Exit 1".into(),
                },
            );
        }
        let rows = document(&state, 40);
        let tools = rows
            .iter()
            .filter(|row| row.to_string().contains("shell"))
            .collect::<Vec<_>>();
        assert_eq!(tools.len(), 2);
        assert!(
            tools
                .iter()
                .all(|line| line.width() <= 38 && line.to_string().ends_with('…'))
        );
        assert!(rows.iter().all(|line| !line.to_string().contains('\x1b')));
        let first = rows
            .iter()
            .position(|row| row.to_string().contains("running"));
        let second = rows
            .iter()
            .position(|row| row.to_string().contains("failed"));
        assert_eq!(first.map(|index| index + 1), second);
    }

    #[test]
    fn streaming_and_committed_markdown_share_one_message_and_keep_padding() {
        let mut state = NativeState {
            live: Some(NativeLive {
                item_id: Some("answer".into()),
                text: "**Ready**\n\n```sh\necho hi\n```".into(),
                truncated: false,
            }),
            ..Default::default()
        };
        let before = document(&state, 60);
        state.upsert(
            "answer".into(),
            "**Ready**\n\n```sh\necho hi\n```".into(),
            NativeEntryKind::Assistant,
        );
        let after = document(&state, 60);
        assert!(state.live.is_none());
        assert_eq!(before, after);
        let output = after
            .iter()
            .map(ToString::to_string)
            .collect::<Vec<_>>()
            .join("\n");
        assert_eq!(output.matches("● BRO").count(), 1);
        assert!(!output.contains("**Ready**") && !output.contains("```"));
        assert!(output.contains("echo hi"));
        assert!(after.iter().all(|line| line.to_string().starts_with("  ")));
    }

    #[test]
    fn model_selection_keeps_conversation_draft_separate() {
        let mut state = NativeState::default();
        state.editor.set_text("unsent draft");
        state.model_editor = Some(Editor::default());
        assert!(!state.can_open_agents());
        assert_eq!(state.editor.text(), "unsent draft");
    }
}
