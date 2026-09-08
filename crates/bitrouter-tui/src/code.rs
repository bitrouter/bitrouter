//! Conversation-first Code state and full-screen renderer.
//!
//! The application owns ACP I/O, reports, clipboard access, and async work.
//! This module retains the conversation projection and returns plain effects.

use std::collections::{HashMap, VecDeque};
use std::io::{self, IsTerminal};

use agent_client_protocol_schema::v1::{
    ContentBlock, ContentChunk, RequestPermissionOutcome, SelectedPermissionOutcome, SessionUpdate,
    TextContent, ToolCall, ToolCallStatus, ToolCallUpdate,
};
use crossterm::cursor::Hide;
use crossterm::event::{Event, KeyCode, KeyEvent, KeyEventKind, KeyModifiers};
use crossterm::execute;
use ratatui::backend::CrosstermBackend;
use ratatui::layout::{Constraint, Layout, Position, Rect};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, Borders, Clear, Paragraph, Wrap};
use ratatui::{Frame, Terminal};
use unicode_segmentation::UnicodeSegmentation as _;
use unicode_width::UnicodeWidthStr as _;

use crate::cost;
use crate::editor::{Edit, Editor};
use crate::journal::{Entry, EntryId, Journal, Voice};
use crate::permission::Prompt;
use crate::render::{self, Registry, ToolContext};
use crate::wrap::wrap;

/// The four persistent conversation facts.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CodeStatus {
    /// A workspace or target label for the header.
    pub title: String,
    /// Resolved selected agent identity.
    pub agent: String,
    /// Confirmed session route.
    pub route: String,
    /// Client-observed lifecycle activity.
    pub activity: String,
}

impl Default for CodeStatus {
    fn default() -> Self {
        Self {
            title: "bitrouter code".to_string(),
            agent: "choose an agent".to_string(),
            route: "unreported".to_string(),
            activity: "choosing agent".to_string(),
        }
    }
}

/// Identity of a command row.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CommandOwner {
    /// BitRouter-owned operation.
    BitRouter,
    /// Command advertised by the active ACP agent.
    Agent,
    /// Local prompt expansion.
    PromptTemplate,
}

impl CommandOwner {
    fn label(self) -> &'static str {
        match self {
            Self::BitRouter => "BitRouter",
            Self::Agent => "Agent",
            Self::PromptTemplate => "Prompt template",
        }
    }
}

/// Work performed after a command is explicitly accepted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CommandTarget {
    /// Typed operation owned by the application action port.
    LocalAction {
        /// Stable action id.
        action: String,
        /// Action arguments.
        args: Vec<String>,
    },
    /// Open the agent selector.
    ChooseAgent,
    /// Open the native session selector.
    OpenSession,
    /// Open ACP-led agent settings.
    Settings,
    /// Open or refresh an application-owned bounded report.
    Report {
        /// Opaque report id.
        id: String,
    },
    /// Send an agent command as prompt text without local re-resolution.
    AgentPrompt {
        /// Exact command prompt.
        prompt: String,
    },
    /// Put an expansion into the composer for review and editing.
    PromptTemplate {
        /// Expanded prompt text.
        prompt: String,
    },
}

/// A palette command supplied by the application.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Command {
    /// Visible spelling and direct typed-local lookup spelling.
    pub label: String,
    /// Short explanation.
    pub detail: String,
    /// Row identity shown to the user.
    pub owner: CommandOwner,
    /// Accepted action.
    pub target: CommandTarget,
    /// Reason it cannot currently run.
    pub unavailable: Option<String>,
}

impl Command {
    /// Construct an enabled command row.
    pub fn new(
        label: impl Into<String>,
        detail: impl Into<String>,
        owner: CommandOwner,
        target: CommandTarget,
    ) -> Self {
        Self {
            label: label.into(),
            detail: detail.into(),
            owner,
            target,
            unavailable: None,
        }
    }

    /// Mark an offered command unavailable with an honest explanation.
    pub fn unavailable(mut self, reason: impl Into<String>) -> Self {
        self.unavailable = Some(reason.into());
        self
    }
}

/// A single application-owned selector row.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SelectorRow {
    /// Opaque result id.
    pub id: String,
    /// Main visible label.
    pub label: String,
    /// Searchable supporting text.
    pub detail: String,
    /// Reason it cannot currently run.
    pub unavailable: Option<String>,
}

impl SelectorRow {
    /// Construct an enabled row.
    pub fn new(id: impl Into<String>, label: impl Into<String>, detail: impl Into<String>) -> Self {
        Self {
            id: id.into(),
            label: label.into(),
            detail: detail.into(),
            unavailable: None,
        }
    }

    /// Mark a visible row unavailable with an explanation.
    pub fn unavailable(mut self, reason: impl Into<String>) -> Self {
        self.unavailable = Some(reason.into());
        self
    }
}

/// A transient searchable selector with application-owned semantics.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selector {
    /// Opaque selector id.
    pub id: String,
    /// Surface title.
    pub title: String,
    /// Capability/scope explanation.
    pub detail: String,
    /// Current choices.
    pub rows: Vec<SelectorRow>,
    /// Whether Enter may explicitly submit a typed custom id/query.
    pub allow_custom: bool,
    /// Explanation rendered for a custom value.
    pub custom_label: String,
}

impl Selector {
    /// Construct a selector from plain data.
    pub fn new(
        id: impl Into<String>,
        title: impl Into<String>,
        detail: impl Into<String>,
        rows: Vec<SelectorRow>,
    ) -> Self {
        Self {
            id: id.into(),
            title: title.into(),
            detail: detail.into(),
            rows,
            allow_custom: false,
            custom_label: String::new(),
        }
    }

    /// Permit explicit custom query confirmation, for native IDs or reports.
    pub fn allow_custom(mut self, label: impl Into<String>) -> Self {
        self.allow_custom = true;
        self.custom_label = label.into();
        self
    }
}

/// A read-only transient inspector retaining all supplied content.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Inspector {
    /// Inspector heading.
    pub title: String,
    /// Exact content retained for search and copy.
    pub content: String,
}

impl Inspector {
    /// Construct an inspector.
    pub fn new(title: impl Into<String>, content: impl Into<String>) -> Self {
        Self {
            title: title.into(),
            content: content.into(),
        }
    }
}

/// End state of an application-owned prompt turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TurnOutcome {
    /// Normal completion, so one queued follow-up may run.
    Completed,
    /// A normal-looking but non-continuable stop reason.
    Stopped(String),
    /// Prompt request failure.
    Failed(String),
    /// Cancellation settled.
    Cancelled,
    /// Connection no longer usable.
    Disconnected,
}

/// Input and lifecycle facts accepted by the pure reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeAction {
    /// Decoded terminal event.
    Event(Event),
    /// Driver began the outstanding prompt.
    TurnStarted,
    /// The driver could not start a just-emitted prompt.
    SubmissionRejected {
        /// The exact prompt the driver did not start.
        prompt: String,
        /// Recoverable diagnostic to show beside the restored draft.
        reason: String,
    },
    /// Driver observed its prompt settle.
    TurnSettled(TurnOutcome),
    /// External editor returned a recoverable replacement or error.
    ExternalEditorFinished(Result<String, String>),
}

/// Application work emitted by the pure reducer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CodeEffect {
    /// Send ordinary draft text as an ACP prompt.
    Submit {
        /// Exact bytes to send.
        prompt: String,
    },
    /// Send an explicitly selected agent command as ACP prompt text.
    AgentPrompt {
        /// Exact bytes to send.
        prompt: String,
    },
    /// Run a BitRouter operation through an injected action port.
    LocalAction {
        /// Stable action id.
        action: String,
        /// Action arguments.
        args: Vec<String>,
    },
    /// Open the app-supplied agent selector.
    ChooseAgent,
    /// Open the app-supplied session selector.
    OpenSession,
    /// Open ACP-led settings.
    Settings,
    /// Show/fetch a bounded report.
    Report {
        /// Opaque report id.
        id: String,
    },
    /// Deliver an explicit selector row or custom query.
    Select {
        /// Selector id.
        selector: String,
        /// Row id or explicitly confirmed custom input.
        id: String,
        /// True only for a deliberate free-text confirmation.
        custom: bool,
    },
    /// Resolve one pending ACP permission request.
    ResolvePermission {
        /// Application request id.
        id: String,
        /// Agent-offered selection or protocol cancellation.
        outcome: RequestPermissionOutcome,
    },
    /// Request cancellation of the active turn.
    Cancel,
    /// Suspend the terminal for the configured external editor.
    ExternalEditor,
    /// Copy retained data through application clipboard support/fallback.
    Copy {
        /// Exact text.
        text: String,
    },
    /// Exit the interactive Code process.
    Exit,
}

/// A process-local follow-up prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QueuedPrompt {
    /// Exact prompt text.
    pub prompt: String,
    /// Origin visible in the queue.
    pub owner: Option<CommandOwner>,
    target: Option<CommandTarget>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum TurnState {
    Ready,
    Submitting,
    Working,
    Cancelling,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadingAnchor {
    entry: EntryId,
    source_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ReadingPosition {
    entry: EntryId,
    source_offset: usize,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Surface {
    Conversation,
    Palette(ChoiceList),
    Selector(ChoiceList),
    Inspector(OpenInspector),
    Permission,
    Queue { selected: usize },
    TranscriptSearch { query: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct ChoiceList {
    kind: ChoiceListKind,
    title: String,
    detail: String,
    query: String,
    choices: Vec<Choice>,
    matches: Vec<usize>,
    selected: Option<usize>,
    allow_custom: bool,
    custom_label: String,
    /// The transient surface to restore when this list closes. A boxed surface
    /// permits the operations root inspector to open a palette without
    /// collapsing back to the conversation.
    return_to: Option<Box<Surface>>,
}

/// One selector that started an asynchronous route or settings mutation.
///
/// Selector acceptance normally closes the transient surface before the
/// application begins its effect. Keeping this bounded snapshot lets an error
/// inspector return to the exact filtered source without making unrelated
/// reports restore an old picker.
#[derive(Debug)]
struct PendingSelectorMutation {
    selector: String,
    source: ChoiceList,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ChoiceListKind {
    Palette { slash: bool },
    Selector { id: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Choice {
    Command(Command),
    Selector(SelectorRow),
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct OpenInspector {
    inspector: Inspector,
    scroll: usize,
    search: String,
    searching: bool,
    /// Surface to restore after a report/error temporarily covers a picker.
    return_to: Option<Box<Surface>>,
    return_to_permission: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct PendingPrompt {
    prompt: String,
    agent_command: bool,
}

#[derive(Debug, Clone)]
struct PendingPermission {
    prompt: Prompt,
    context: Option<ToolCallUpdate>,
}

/// Pure state for one active native conversation.
#[derive(Debug)]
pub struct CodeState {
    status: CodeStatus,
    journal: Journal,
    journal_revision: u64,
    editor: Editor,
    session_active: bool,
    turn: TurnState,
    pending_prompt: Option<PendingPrompt>,
    commands: Vec<Command>,
    typed_commands: Vec<crate::machine::Command>,
    prompt_commands: Vec<crate::machine::PromptCommand>,
    selectors: Vec<Selector>,
    pending_selector_mutation: Option<PendingSelectorMutation>,
    permissions: VecDeque<PendingPermission>,
    permission_selected: Option<usize>,
    permission_return: Option<Surface>,
    queue: VecDeque<QueuedPrompt>,
    selected_command: Option<(String, CommandTarget, CommandOwner)>,
    surface: Surface,
    follow_live: bool,
    reading_anchor: Option<ReadingAnchor>,
    reading_positions: Vec<ReadingPosition>,
    reading_layout_revision: u64,
    reading_page: usize,
    new_activity: bool,
    notice: Option<String>,
    dispatch_after_permissions: bool,
    operations_only: bool,
    operations_root: Option<Inspector>,
}

impl Default for CodeState {
    fn default() -> Self {
        Self::new(CodeStatus::default())
    }
}

impl CodeState {
    /// Construct an empty conversation state.
    pub fn new(status: CodeStatus) -> Self {
        Self {
            status,
            journal: Journal::default(),
            journal_revision: 0,
            editor: Editor::default(),
            session_active: false,
            turn: TurnState::Ready,
            pending_prompt: None,
            commands: Vec::new(),
            typed_commands: Vec::new(),
            prompt_commands: Vec::new(),
            selectors: Vec::new(),
            pending_selector_mutation: None,
            permissions: VecDeque::new(),
            permission_selected: None,
            permission_return: None,
            queue: VecDeque::new(),
            selected_command: None,
            surface: Surface::Conversation,
            follow_live: true,
            reading_anchor: None,
            reading_positions: Vec::new(),
            reading_layout_revision: 0,
            reading_page: 1,
            new_activity: false,
            notice: None,
            dispatch_after_permissions: false,
            operations_only: false,
            operations_root: None,
        }
    }

    /// Retained ACP transcript projection.
    pub fn journal(&self) -> &Journal {
        &self.journal
    }

    /// Current multiline draft editor.
    pub fn editor(&self) -> &Editor {
        &self.editor
    }

    /// Current four-field status data.
    pub fn status(&self) -> &CodeStatus {
        &self.status
    }

    /// Replace lifecycle/status facts supplied by the driver.
    pub fn set_status(&mut self, status: CodeStatus) {
        self.status = status;
    }

    /// Mark whether an ACP session exists for prompt submission.
    pub fn set_session_active(&mut self, active: bool) {
        self.session_active = active;
        self.refresh_open_palettes();
    }

    /// Show a short recoverable diagnostic.
    pub fn set_notice(&mut self, notice: impl Into<String>) {
        self.notice = Some(notice.into());
    }

    /// Replace BitRouter and prompt-template palette rows.
    pub fn set_commands(&mut self, commands: Vec<Command>) {
        self.commands = commands;
        self.refresh_open_palettes();
    }

    /// Set the canonical typed-slash resolver data.
    ///
    /// The application already constructs these rows for its existing command
    /// surface. Reusing the resolver preserves longest-name matching, aliases,
    /// prompt-template arguments, and local-over-agent precedence.
    pub fn set_typed_commands(
        &mut self,
        commands: Vec<crate::machine::Command>,
        prompt_commands: Vec<crate::machine::PromptCommand>,
    ) {
        self.typed_commands = commands;
        self.prompt_commands = prompt_commands;
    }

    /// Replace application-supplied capability selectors.
    pub fn set_selectors(&mut self, selectors: Vec<Selector>) {
        self.selectors = selectors;
        let main_available = refresh_selector_surface(&mut self.surface, &self.selectors);
        let return_available = self
            .permission_return
            .as_mut()
            .map(|surface| refresh_selector_surface(surface, &self.selectors))
            .unwrap_or(true);
        let pending_available = self
            .pending_selector_mutation
            .as_mut()
            .map(|pending| refresh_selector_list(&mut pending.source, &self.selectors))
            .unwrap_or(true);
        if !pending_available {
            self.pending_selector_mutation = None;
        }
        if !main_available || !return_available || !pending_available {
            self.notice = Some("That selector is no longer available in this session".to_string());
        }
        self.refresh_open_palettes();
    }

    /// Mark a selector-backed route or settings mutation as confirmed.
    ///
    /// The application calls this after the corresponding asynchronous result
    /// succeeds. It only clears a matching snapshot, so a report or another
    /// operation cannot discard a different pending selector restoration.
    pub fn selector_mutation_succeeded(&mut self, selector: &str) {
        if self
            .pending_selector_mutation
            .as_ref()
            .is_some_and(|pending| pending.selector == selector)
        {
            self.pending_selector_mutation = None;
        }
    }

    /// Restore the exact selector that started a failed route or settings mutation.
    ///
    /// Call this immediately before opening an error inspector. The inspector
    /// will then return to the saved query, selected row, and prior transient
    /// surface. Returns whether a matching selector was restored.
    pub fn selector_mutation_failed(&mut self, selector: &str) -> bool {
        let Some(pending) = self.pending_selector_mutation.take() else {
            return false;
        };
        if pending.selector != selector {
            self.pending_selector_mutation = Some(pending);
            return false;
        }
        self.surface = Surface::Selector(pending.source);
        true
    }

    /// Open a supplied selector, or record an honest unavailable notice.
    pub fn open_selector(&mut self, id: &str) -> bool {
        let Some(selector) = self.selectors.iter().find(|selector| selector.id == id) else {
            self.notice = Some(format!("{id} is unavailable in this session"));
            return false;
        };
        let choices = selector
            .rows
            .iter()
            .cloned()
            .map(Choice::Selector)
            .collect::<Vec<_>>();
        let return_to = self.transient_return_target();
        self.surface = Surface::Selector(ChoiceList::selector(selector, choices, return_to));
        true
    }

    /// Open a full-content temporary inspector.
    pub fn open_inspector(&mut self, inspector: Inspector) {
        let return_to = self.transient_return_target();
        self.open_inspector_returning_to(inspector, return_to);
    }

    fn open_inspector_returning_to(
        &mut self,
        inspector: Inspector,
        return_to: Option<Box<Surface>>,
    ) {
        self.surface = Surface::Inspector(OpenInspector {
            inspector,
            scroll: 0,
            search: String::new(),
            searching: false,
            return_to,
            return_to_permission: false,
        });
    }

    /// Enter the read-only remote or local operations root.
    ///
    /// Closing this root exits; closing a nested inspector returns to it. The
    /// composer remains disabled for the entire operations-only surface.
    pub fn operations_root(&mut self, inspector: Inspector) {
        self.operations_only = true;
        self.operations_root = Some(inspector.clone());
        self.open_inspector_returning_to(inspector, None);
    }

    /// Toggle read-only operations mode when no ACP session exists.
    pub fn set_operations_only(&mut self, operations_only: bool) {
        self.operations_only = operations_only;
        if !operations_only {
            self.operations_root = None;
        }
        self.refresh_open_palettes();
    }

    /// Reset the native-session projection while retaining the draft.
    ///
    /// The queue belongs to the prior native session, so the caller must make
    /// an explicit discard decision before changing sessions.
    pub fn reset_session(&mut self) -> bool {
        if !self.queue.is_empty() {
            self.notice =
                Some("Resolve or discard queued prompts before changing sessions".to_string());
            return false;
        }
        self.journal = Journal::default();
        self.journal_revision = self.journal_revision.saturating_add(1);
        self.pending_selector_mutation = None;
        self.permissions.clear();
        self.permission_selected = None;
        self.permission_return = None;
        self.turn = TurnState::Ready;
        self.pending_prompt = None;
        self.dispatch_after_permissions = false;
        self.session_active = false;
        self.surface = Surface::Conversation;
        self.follow_live = true;
        self.reading_anchor = None;
        self.reading_positions.clear();
        self.reading_layout_revision = 0;
        self.reading_page = 1;
        self.new_activity = false;
        self.selected_command = None;
        true
    }

    /// Explicitly discard all process-local queued prompts.
    pub fn discard_queue(&mut self) {
        self.queue.clear();
        self.refresh_open_palettes();
    }

    /// Number of prompts awaiting a normal turn completion.
    pub fn queue_len(&self) -> usize {
        self.queue.len()
    }

    /// Apply one raw ACP update to the retained journal.
    pub fn apply(&mut self, update: SessionUpdate) {
        self.journal.apply(update);
        self.journal_revision = self.journal_revision.saturating_add(1);
        if !self.follow_live {
            self.new_activity = true;
        }
        self.refresh_open_palettes();
    }

    fn sync_reading_layout(&mut self, revision: u64, positions: &[ReadingPosition], page: usize) {
        self.reading_page = page.max(1);
        if self.reading_layout_revision == revision {
            return;
        }
        self.reading_positions = positions.to_vec();
        self.reading_layout_revision = revision;
    }

    /// Queue a permission request by identity.
    ///
    /// Permission arrival never focuses an option. F2 does, which prevents
    /// buffered composer input from becoming consent.
    pub fn receive_permission(&mut self, prompt: Prompt) -> Vec<CodeEffect> {
        self.receive_permission_with_optional_context(prompt, None)
    }

    /// Queue a permission with the raw structured tool context the agent sent.
    ///
    /// The compact permission surface shows a bounded summary and F4 opens the
    /// complete retained context. The request identity remains the Prompt id;
    /// this auxiliary context never replaces or synthesizes an option.
    pub fn receive_permission_with_context(
        &mut self,
        prompt: Prompt,
        context: ToolCallUpdate,
    ) -> Vec<CodeEffect> {
        self.receive_permission_with_optional_context(prompt, Some(context))
    }

    fn receive_permission_with_optional_context(
        &mut self,
        prompt: Prompt,
        context: Option<ToolCallUpdate>,
    ) -> Vec<CodeEffect> {
        if self.turn == TurnState::Cancelling {
            return vec![CodeEffect::ResolvePermission {
                id: prompt.id().to_string(),
                outcome: RequestPermissionOutcome::Cancelled,
            }];
        }
        self.permissions
            .push_back(PendingPermission { prompt, context });
        self.notice = Some(format!(
            "Permission needed · F2 focuses oldest pending request ({})",
            self.permissions.len()
        ));
        self.refresh_open_palettes();
        Vec::new()
    }
}

impl CodeState {
    /// Apply terminal input or lifecycle data and return application effects.
    pub fn step(&mut self, action: CodeAction) -> Vec<CodeEffect> {
        match action {
            CodeAction::Event(event) => self.event(&event),
            CodeAction::TurnStarted => {
                if let Some(pending) = self.pending_prompt.take() {
                    let prompt = pending.prompt;
                    self.finish_journal_stream();
                    self.apply(SessionUpdate::UserMessageChunk(ContentChunk::new(
                        ContentBlock::Text(TextContent::new(prompt.clone())),
                    )));
                    self.finish_journal_stream();
                    self.editor.push_history(prompt);
                    self.turn = TurnState::Working;
                    self.refresh_open_palettes();
                }
                Vec::new()
            }
            CodeAction::SubmissionRejected { prompt, reason } => {
                self.submission_rejected(prompt, reason)
            }
            CodeAction::TurnSettled(outcome) => self.turn_settled(outcome),
            CodeAction::ExternalEditorFinished(result) => match result {
                Ok(text) => {
                    self.editor.set_text(text);
                    self.notice = None;
                    Vec::new()
                }
                Err(error) => {
                    self.notice = Some(format!("External editor failed: {error}"));
                    Vec::new()
                }
            },
        }
    }

    fn event(&mut self, event: &Event) -> Vec<CodeEffect> {
        if let Some(key) = pressed(event)
            && key.code == KeyCode::F(2)
            && !self.permissions.is_empty()
        {
            if !matches!(&self.surface, Surface::Permission) {
                let from_permission_context = matches!(
                    &self.surface,
                    Surface::Inspector(OpenInspector {
                        return_to_permission: true,
                        ..
                    })
                );
                if !from_permission_context {
                    self.permission_return = Some(self.surface.clone());
                }
                self.permission_selected = None;
                self.surface = Surface::Permission;
            }
            return Vec::new();
        }
        match self.surface {
            Surface::Conversation => self.conversation_event(event),
            Surface::Palette(_) | Surface::Selector(_) => self.choice_event(event),
            Surface::Inspector(_) => self.inspector_event(event),
            Surface::Permission => self.permission_event(event),
            Surface::Queue { .. } => self.queue_event(event),
            Surface::TranscriptSearch { .. } => self.transcript_search_event(event),
        }
    }

    fn conversation_event(&mut self, event: &Event) -> Vec<CodeEffect> {
        if self.operations_only {
            let Some(key) = pressed(event) else {
                return Vec::new();
            };
            if control(key, 'p') {
                self.open_palette(false, String::new());
                return Vec::new();
            }
            if key.code == KeyCode::Esc
                || (key.code == KeyCode::Char('d') && key.modifiers.contains(KeyModifiers::CONTROL))
            {
                return vec![CodeEffect::Exit];
            }
            return Vec::new();
        }

        if let Event::Paste(text) = event {
            self.editor.paste(text);
            self.clear_stale_command();
            return Vec::new();
        }
        let Some(key) = pressed(event) else {
            return Vec::new();
        };

        if key.code == KeyCode::F(3) && !self.queue.is_empty() {
            self.surface = Surface::Queue { selected: 0 };
            return Vec::new();
        }
        if key.code == KeyCode::F(4) {
            self.inspect_anchor();
            return Vec::new();
        }
        if control(key, 'p') {
            self.open_palette(false, String::new());
            return Vec::new();
        }
        if control(key, 'f') {
            self.surface = Surface::TranscriptSearch {
                query: String::new(),
            };
            return Vec::new();
        }
        if control(key, 'y') {
            return self.copy_anchor();
        }
        if control(key, 'g') && self.turn == TurnState::Ready && self.permissions.is_empty() {
            return vec![CodeEffect::ExternalEditor];
        }
        if control(key, 'l') {
            return Vec::new();
        }
        if key.code == KeyCode::End && key.modifiers.contains(KeyModifiers::CONTROL) {
            self.return_to_live();
            return Vec::new();
        }
        match key.code {
            KeyCode::PageUp => {
                self.read_previous();
                return Vec::new();
            }
            KeyCode::PageDown => {
                self.read_next();
                return Vec::new();
            }
            KeyCode::Tab if matches!(self.turn, TurnState::Submitting | TurnState::Working) => {
                self.queue_current();
                return Vec::new();
            }
            KeyCode::Esc if self.turn == TurnState::Working => return self.cancel(),
            KeyCode::Esc if self.turn == TurnState::Submitting => {
                self.notice = Some("Waiting for prompt submission to start".to_string());
                return Vec::new();
            }
            KeyCode::Char('c') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return match self.turn {
                    TurnState::Working => self.cancel(),
                    TurnState::Submitting => {
                        let prompt = self
                            .pending_prompt
                            .as_ref()
                            .map(|pending| pending.prompt.clone())
                            .unwrap_or_default();
                        self.submission_rejected(prompt, "submission cancelled".to_string())
                    }
                    TurnState::Cancelling => Vec::new(),
                    TurnState::Ready if self.editor.text().is_empty() => vec![CodeEffect::Exit],
                    TurnState::Ready => {
                        self.editor.clear();
                        self.selected_command = None;
                        self.notice = Some("Draft cleared".to_string());
                        Vec::new()
                    }
                };
            }
            KeyCode::Char('d') if key.modifiers.contains(KeyModifiers::CONTROL) => {
                return match self.turn {
                    TurnState::Ready if self.editor.text().is_empty() => vec![CodeEffect::Exit],
                    TurnState::Ready => {
                        self.notice =
                            Some("Clear the draft or use Ctrl-C before leaving".to_string());
                        Vec::new()
                    }
                    TurnState::Submitting | TurnState::Working | TurnState::Cancelling => {
                        self.notice = Some("Finish or cancel the turn before leaving".to_string());
                        Vec::new()
                    }
                };
            }
            KeyCode::Enter
                if !key
                    .modifiers
                    .intersects(KeyModifiers::SHIFT | KeyModifiers::ALT)
                    && matches!(self.turn, TurnState::Submitting | TurnState::Working) =>
            {
                self.notice = Some("Tab queues for the next turn; Esc interrupts".to_string());
                return Vec::new();
            }
            KeyCode::Esc if self.turn == TurnState::Ready => {
                self.selected_command = None;
                return Vec::new();
            }
            _ => {}
        }

        match self.editor.apply(*key) {
            Edit::Changed => {
                self.clear_stale_command();
                if self.editor.text().starts_with('/') {
                    let query = self
                        .editor
                        .text()
                        .strip_prefix('/')
                        .map_or_else(String::new, ToString::to_string);
                    self.open_palette(true, query);
                }
                Vec::new()
            }
            Edit::Submitted if self.turn == TurnState::Ready => self.submit_current(),
            Edit::Submitted => {
                self.notice = Some("Tab queues for the next turn; Esc interrupts".to_string());
                Vec::new()
            }
            Edit::OpenExternalEditor => vec![CodeEffect::ExternalEditor],
            Edit::ExitRequested => {
                self.notice = Some("Clear the draft or use Ctrl-C before leaving".to_string());
                Vec::new()
            }
            Edit::Ended => vec![CodeEffect::Exit],
            Edit::Ignored | Edit::Redrawn => Vec::new(),
        }
    }

    fn choice_event(&mut self, event: &Event) -> Vec<CodeEffect> {
        let Some(key) = pressed(event) else {
            return Vec::new();
        };
        if key.code == KeyCode::Esc || control(key, 'c') {
            self.close_choice();
            return Vec::new();
        }
        let mut accepted = None;
        if let Surface::Palette(list) | Surface::Selector(list) = &mut self.surface {
            match key.code {
                KeyCode::Up => list.move_by(-1),
                KeyCode::Down => list.move_by(1),
                KeyCode::PageUp => list.move_by(-8),
                KeyCode::PageDown => list.move_by(8),
                KeyCode::Home => list.first(),
                KeyCode::End => list.last(),
                KeyCode::Backspace => {
                    list.query.pop();
                    list.filter();
                    list.sync_slash_draft(&mut self.editor);
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::ALT) => {
                    list.query.push(character);
                    list.filter();
                    list.sync_slash_draft(&mut self.editor);
                }
                KeyCode::Enter => accepted = list.accepted(),
                _ => {}
            }
        }
        match accepted {
            Some(Accepted::Choice(choice, kind)) => self.accept_choice(choice, kind),
            Some(Accepted::Custom { selector, value }) => {
                self.remember_selector_mutation(&selector);
                self.close_choice();
                vec![CodeEffect::Select {
                    selector,
                    id: value,
                    custom: true,
                }]
            }
            None => Vec::new(),
        }
    }

    fn accept_choice(&mut self, choice: Choice, kind: ChoiceListKind) -> Vec<CodeEffect> {
        match choice {
            Choice::Selector(row) => {
                if let Some(reason) = row.unavailable {
                    self.notice = Some(reason);
                    return Vec::new();
                }
                let ChoiceListKind::Selector { id } = kind else {
                    return Vec::new();
                };
                self.remember_selector_mutation(&id);
                self.close_choice();
                vec![CodeEffect::Select {
                    selector: id,
                    id: row.id,
                    custom: false,
                }]
            }
            Choice::Command(command) => {
                if let Some(reason) = command.unavailable {
                    self.notice = Some(reason);
                    return Vec::new();
                }
                self.close_choice();
                self.accept_command(command)
            }
        }
    }

    fn accept_command(&mut self, command: Command) -> Vec<CodeEffect> {
        match command.target.clone() {
            CommandTarget::PromptTemplate { prompt } => {
                let expanded = prompt.replace("$ARGUMENTS", "");
                self.editor.set_text(expanded.clone());
                self.selected_command = Some((
                    expanded.clone(),
                    CommandTarget::PromptTemplate { prompt: expanded },
                    command.owner,
                ));
                Vec::new()
            }
            CommandTarget::AgentPrompt { prompt } => {
                self.editor.set_text(prompt.clone());
                self.selected_command = Some((prompt, command.target, command.owner));
                if self.turn == TurnState::Ready {
                    self.submit_current()
                } else {
                    self.notice =
                        Some("Tab queues this agent command for the next turn".to_string());
                    Vec::new()
                }
            }
            target => self.effect_for_target(target),
        }
    }

    fn remember_selector_mutation(&mut self, selector: &str) {
        if !is_async_mutation_selector(selector) {
            return;
        }
        self.pending_selector_mutation = match &self.surface {
            Surface::Selector(source) => Some(PendingSelectorMutation {
                selector: selector.to_string(),
                source: source.clone(),
            }),
            Surface::Conversation
            | Surface::Palette(_)
            | Surface::Inspector(_)
            | Surface::Permission
            | Surface::Queue { .. }
            | Surface::TranscriptSearch { .. } => None,
        };
    }

    fn effect_for_target(&mut self, target: CommandTarget) -> Vec<CodeEffect> {
        match target {
            CommandTarget::LocalAction { action, args } => {
                vec![CodeEffect::LocalAction { action, args }]
            }
            CommandTarget::ChooseAgent => vec![CodeEffect::ChooseAgent],
            CommandTarget::OpenSession => vec![CodeEffect::OpenSession],
            CommandTarget::Settings => vec![CodeEffect::Settings],
            CommandTarget::Report { id } => vec![CodeEffect::Report { id }],
            CommandTarget::AgentPrompt { prompt } => self.begin_prompt(prompt, true),
            CommandTarget::PromptTemplate { prompt } => {
                self.editor.set_text(prompt);
                Vec::new()
            }
        }
    }

    fn permission_event(&mut self, event: &Event) -> Vec<CodeEffect> {
        let Some(key) = pressed(event) else {
            return Vec::new();
        };
        if control(key, 'l') {
            return Vec::new();
        }
        if control(key, 'c') {
            return self.cancel();
        }
        if key.code == KeyCode::F(4) {
            self.inspect_permission_context();
            return Vec::new();
        }

        let option_count = self
            .permissions
            .front()
            .map(|pending| pending.prompt.options().len())
            .unwrap_or_default();
        if self.permissions.is_empty() {
            self.restore_after_permission();
            return Vec::new();
        }
        if key.code == KeyCode::Esc {
            let outcome = self
                .permissions
                .front()
                .map(|pending| pending.prompt.unanswered());
            if let Some(outcome) = outcome {
                return self.resolve_oldest_permission(outcome);
            }
            self.surface = Surface::Conversation;
            return Vec::new();
        }
        match key.code {
            KeyCode::Char(number) if number.is_ascii_digit() => {
                self.permission_selected = number
                    .to_digit(10)
                    .and_then(|value| value.checked_sub(1))
                    .and_then(|value| usize::try_from(value).ok())
                    .filter(|index| *index < option_count);
            }
            KeyCode::Up => {
                self.permission_selected =
                    previous_selection(self.permission_selected, option_count);
            }
            KeyCode::Down => {
                self.permission_selected = next_selection(self.permission_selected, option_count);
            }
            KeyCode::Enter => {
                let Some(index) = self.permission_selected else {
                    self.notice = Some("Select a permission option before confirming".to_string());
                    return Vec::new();
                };
                let outcome = self.permissions.front().and_then(|pending| {
                    pending.prompt.options().get(index).map(|option| {
                        RequestPermissionOutcome::Selected(SelectedPermissionOutcome::new(
                            option.option_id.clone(),
                        ))
                    })
                });
                let Some(outcome) = outcome else {
                    self.permission_selected = None;
                    return Vec::new();
                };
                return self.resolve_oldest_permission(outcome);
            }
            _ => {}
        }
        Vec::new()
    }

    fn resolve_oldest_permission(&mut self, outcome: RequestPermissionOutcome) -> Vec<CodeEffect> {
        let Some(pending) = self.permissions.pop_front() else {
            self.restore_after_permission();
            self.permission_selected = None;
            return Vec::new();
        };
        self.permission_selected = None;
        self.restore_after_permission();
        let mut effects = vec![CodeEffect::ResolvePermission {
            id: pending.prompt.id().to_string(),
            outcome,
        }];
        if self.permissions.is_empty() && self.dispatch_after_permissions {
            self.dispatch_after_permissions = false;
            effects.extend(self.dispatch_next());
        }
        self.refresh_open_palettes();
        effects
    }

    fn restore_after_permission(&mut self) {
        self.surface = self
            .permission_return
            .take()
            .unwrap_or(Surface::Conversation);
    }

    fn queue_event(&mut self, event: &Event) -> Vec<CodeEffect> {
        let Some(key) = pressed(event) else {
            return Vec::new();
        };
        let Surface::Queue { selected } = &mut self.surface else {
            return Vec::new();
        };
        if key.code == KeyCode::Esc || control(key, 'c') {
            self.surface = Surface::Conversation;
            return Vec::new();
        }
        match key.code {
            KeyCode::Up => *selected = selected.saturating_sub(1),
            KeyCode::Down => {
                *selected = selected
                    .saturating_add(1)
                    .min(self.queue.len().saturating_sub(1));
            }
            KeyCode::Delete | KeyCode::Backspace => {
                let index = *selected;
                let _ = self.queue.remove(index);
                if self.queue.is_empty() {
                    self.surface = Surface::Conversation;
                } else {
                    *selected = (*selected).min(self.queue.len().saturating_sub(1));
                }
            }
            KeyCode::Enter | KeyCode::Char('e') => {
                let index = *selected;
                if let Some(item) = self.queue.remove(index) {
                    self.editor.set_text(item.prompt);
                    self.selected_command = item.target.map(|target| {
                        (
                            self.editor.text().to_string(),
                            target,
                            item.owner.unwrap_or(CommandOwner::Agent),
                        )
                    });
                }
                self.surface = Surface::Conversation;
            }
            _ => {}
        }
        Vec::new()
    }

    fn transcript_search_event(&mut self, event: &Event) -> Vec<CodeEffect> {
        let Some(key) = pressed(event) else {
            return Vec::new();
        };
        let mut updated_query = None;
        let mut close = false;
        if let Surface::TranscriptSearch { query } = &mut self.surface {
            match key.code {
                KeyCode::Esc | KeyCode::Enter => close = true,
                KeyCode::Backspace => {
                    query.pop();
                    updated_query = Some(query.clone());
                }
                KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                    query.push(character);
                    updated_query = Some(query.clone());
                }
                _ => {}
            }
        }
        if control(key, 'c') {
            close = true;
        }
        if let Some(query) = updated_query {
            self.search_transcript(&query);
        }
        if close {
            self.surface = Surface::Conversation;
        }
        Vec::new()
    }

    fn inspector_event(&mut self, event: &Event) -> Vec<CodeEffect> {
        let Some(key) = pressed(event) else {
            return Vec::new();
        };
        if control(key, 'p') {
            let return_to = self.surface.clone();
            self.open_palette_returning_to(false, String::new(), Some(Box::new(return_to)));
            return Vec::new();
        }
        let mut close = false;
        let mut copy = None;
        if let Surface::Inspector(inspector) = &mut self.surface {
            if control(key, 'c') {
                close = true;
            } else if key.code == KeyCode::Esc {
                if inspector.searching {
                    inspector.searching = false;
                } else {
                    close = true;
                }
            } else if control(key, 'f') {
                inspector.searching = true;
            } else if control(key, 'y') {
                copy = Some(inspector.inspector.content.clone());
            } else if inspector.searching {
                match key.code {
                    KeyCode::Enter => inspector.searching = false,
                    KeyCode::Backspace => {
                        inspector.search.pop();
                        if let Some(line) =
                            first_match_line(&inspector.inspector.content, &inspector.search)
                        {
                            inspector.scroll = line;
                        }
                    }
                    KeyCode::Char(character) if !key.modifiers.contains(KeyModifiers::CONTROL) => {
                        inspector.search.push(character);
                        if let Some(line) =
                            first_match_line(&inspector.inspector.content, &inspector.search)
                        {
                            inspector.scroll = line;
                        }
                    }
                    _ => {}
                }
            } else {
                match key.code {
                    KeyCode::Up => inspector.scroll = inspector.scroll.saturating_sub(1),
                    KeyCode::Down => inspector.scroll = inspector.scroll.saturating_add(1),
                    KeyCode::PageUp => inspector.scroll = inspector.scroll.saturating_sub(8),
                    KeyCode::PageDown => inspector.scroll = inspector.scroll.saturating_add(8),
                    KeyCode::Home => inspector.scroll = 0,
                    KeyCode::End => inspector.scroll = inspector.inspector.content.lines().count(),
                    _ => {}
                }
            }
        }
        if close {
            let (return_to_permission, return_to) = match &self.surface {
                Surface::Inspector(inspector) => {
                    (inspector.return_to_permission, inspector.return_to.clone())
                }
                _ => (false, None),
            };
            if let Some(root) = self.operations_root.clone() {
                let closing_root = matches!(
                    &self.surface,
                    Surface::Inspector(current) if current.inspector == root
                );
                if closing_root {
                    return vec![CodeEffect::Exit];
                }
                self.open_inspector_returning_to(root, None);
            } else if return_to_permission {
                self.surface = Surface::Permission;
            } else {
                self.surface = return_to.map_or(Surface::Conversation, |surface| *surface);
            }
        }
        match copy {
            Some(text) => vec![CodeEffect::Copy { text }],
            None => Vec::new(),
        }
    }

    fn submit_current(&mut self) -> Vec<CodeEffect> {
        let text = self.editor.text().to_string();
        if text.trim().is_empty() {
            self.editor.clear();
            self.selected_command = None;
            return Vec::new();
        }
        if let Some((selected, target, _owner)) = self.selected_command.clone()
            && selected == text
        {
            self.selected_command = None;
            if let CommandTarget::PromptTemplate { prompt } = target {
                return self.begin_prompt(prompt, false);
            }
            return self.effect_for_target(target);
        }
        if !self.typed_commands.is_empty() {
            match crate::machine::resolve(&self.typed_commands, &self.prompt_commands, &text) {
                crate::machine::Resolution::Action { action, args }
                | crate::machine::Resolution::Owned { action, args } => {
                    return vec![CodeEffect::LocalAction {
                        action: action.to_string(),
                        args,
                    }];
                }
                crate::machine::Resolution::Unavailable(reason) => {
                    self.notice = Some(reason.to_string());
                    return Vec::new();
                }
                crate::machine::Resolution::Expand(prompt) => {
                    return self.begin_prompt(prompt, false);
                }
                crate::machine::Resolution::Prompt(prompt) => {
                    return self.begin_prompt(prompt, false);
                }
            }
        }
        if let Some(command) = self
            .commands
            .iter()
            .find(|command| {
                command.owner == CommandOwner::BitRouter && command.label == text.trim()
            })
            .cloned()
        {
            if let Some(reason) = command.unavailable {
                self.notice = Some(reason);
                return Vec::new();
            }
            return self.effect_for_target(command.target);
        }
        self.begin_prompt(text, false)
    }

    fn begin_prompt(&mut self, prompt: String, agent_command: bool) -> Vec<CodeEffect> {
        if self.operations_only {
            self.notice = Some("Remote operations are read-only".to_string());
            return Vec::new();
        }
        if !self.session_active {
            self.notice = Some("Choose an agent before sending this draft".to_string());
            return vec![CodeEffect::ChooseAgent];
        }
        self.editor.clear();
        self.selected_command = None;
        self.pending_prompt = Some(PendingPrompt {
            prompt: prompt.clone(),
            agent_command,
        });
        self.turn = TurnState::Submitting;
        self.return_to_live();
        self.refresh_open_palettes();
        if agent_command {
            vec![CodeEffect::AgentPrompt { prompt }]
        } else {
            vec![CodeEffect::Submit { prompt }]
        }
    }

    fn submission_rejected(&mut self, prompt: String, reason: String) -> Vec<CodeEffect> {
        let pending = self.pending_prompt.take();
        let agent_command = pending
            .as_ref()
            .is_some_and(|pending| pending.agent_command);
        let restored = if prompt.is_empty() {
            pending
                .as_ref()
                .map(|pending| pending.prompt.clone())
                .unwrap_or_default()
        } else {
            prompt
        };
        self.turn = TurnState::Ready;
        self.dispatch_after_permissions = false;
        if self.editor.text().is_empty() {
            self.editor.set_text(restored.clone());
            if agent_command {
                self.selected_command = Some((
                    restored.clone(),
                    CommandTarget::AgentPrompt { prompt: restored },
                    CommandOwner::Agent,
                ));
            }
        } else if !restored.is_empty() {
            self.queue.push_front(QueuedPrompt {
                prompt: restored.clone(),
                owner: agent_command.then_some(CommandOwner::Agent),
                target: agent_command.then_some(CommandTarget::AgentPrompt { prompt: restored }),
            });
        }
        self.notice = Some(format!("Prompt was not started: {reason}"));
        self.refresh_open_palettes();
        Vec::new()
    }

    fn queue_current(&mut self) {
        let prompt = self.editor.text().to_string();
        if prompt.trim().is_empty() {
            self.notice = Some("Write a follow-up before queueing it".to_string());
            return;
        }
        let selected = self
            .selected_command
            .take()
            .filter(|(text, _, _)| text == &prompt);
        let queued = match selected {
            Some((_, target, owner)) => QueuedPrompt {
                prompt,
                owner: Some(owner),
                target: Some(target),
            },
            None => match self.queueable_prompt(&prompt) {
                Some(queued) => queued,
                None => return,
            },
        };
        self.queue.push_back(queued);
        self.editor.clear();
        self.notice = Some(format!("Queued for the next turn ({})", self.queue.len()));
    }

    fn queueable_prompt(&mut self, prompt: &str) -> Option<QueuedPrompt> {
        if !self.typed_commands.is_empty() {
            match crate::machine::resolve(&self.typed_commands, &self.prompt_commands, prompt) {
                crate::machine::Resolution::Action { .. }
                | crate::machine::Resolution::Owned { .. }
                | crate::machine::Resolution::Unavailable(_) => {
                    self.notice =
                        Some("BitRouter operations cannot be queued as agent prompts".to_string());
                    return None;
                }
                crate::machine::Resolution::Expand(expanded) => {
                    if expanded.trim().is_empty() {
                        self.notice =
                            Some("Prompt template expands to an empty follow-up".to_string());
                        return None;
                    }
                    return Some(QueuedPrompt {
                        prompt: expanded.clone(),
                        owner: Some(CommandOwner::PromptTemplate),
                        target: Some(CommandTarget::PromptTemplate { prompt: expanded }),
                    });
                }
                crate::machine::Resolution::Prompt(prompt) => {
                    if self.agent_command_available(&prompt) {
                        return Some(QueuedPrompt {
                            prompt: prompt.clone(),
                            owner: Some(CommandOwner::Agent),
                            target: Some(CommandTarget::AgentPrompt { prompt }),
                        });
                    }
                    return Some(QueuedPrompt {
                        prompt,
                        owner: None,
                        target: None,
                    });
                }
            }
        }
        if self.commands.iter().any(|command| {
            command.owner == CommandOwner::BitRouter && command.label == prompt.trim()
        }) {
            self.notice =
                Some("BitRouter operations cannot be queued as agent prompts".to_string());
            return None;
        }
        let agent_command = self.agent_command_available(prompt);
        Some(QueuedPrompt {
            prompt: prompt.to_string(),
            owner: agent_command.then_some(CommandOwner::Agent),
            target: agent_command.then(|| CommandTarget::AgentPrompt {
                prompt: prompt.to_string(),
            }),
        })
    }

    fn turn_settled(&mut self, outcome: TurnOutcome) -> Vec<CodeEffect> {
        self.finish_journal_stream();
        let was_cancelling = self.turn == TurnState::Cancelling;
        self.turn = TurnState::Ready;
        self.pending_prompt = None;
        self.dispatch_after_permissions = false;
        if outcome == TurnOutcome::Disconnected {
            self.clear_disconnected_permissions();
        }
        self.refresh_open_palettes();
        match outcome {
            TurnOutcome::Completed => {
                if was_cancelling {
                    self.notice = Some(
                        "Turn completed after cancellation request. Queue is paused.".to_string(),
                    );
                    return Vec::new();
                }
                self.notice = Some("Turn completed".to_string());
                if self.permissions.is_empty() {
                    return self.dispatch_next();
                }
                self.dispatch_after_permissions = !self.queue.is_empty();
            }
            TurnOutcome::Stopped(reason) => {
                self.notice = Some(format!("Turn stopped: {reason}. Queue is paused."));
            }
            TurnOutcome::Failed(error) => {
                self.notice = Some(format!("Turn failed: {error}. Queue is paused."));
            }
            TurnOutcome::Cancelled => {
                self.notice = Some("Turn cancelled. Queue is paused.".to_string());
            }
            TurnOutcome::Disconnected => {
                self.notice = Some("Disconnected. Queue is paused.".to_string());
            }
        }
        Vec::new()
    }

    fn dispatch_next(&mut self) -> Vec<CodeEffect> {
        let Some(next) = self.queue.front().cloned() else {
            return Vec::new();
        };
        if matches!(next.owner, Some(CommandOwner::Agent))
            && !self.agent_command_available(&next.prompt)
        {
            self.notice =
                Some("Queued agent command is no longer advertised; queue is paused".to_string());
            return Vec::new();
        }
        let _ = self.queue.pop_front();
        match next.target {
            Some(CommandTarget::AgentPrompt { prompt }) => self.begin_prompt(prompt, true),
            Some(CommandTarget::PromptTemplate { prompt }) => self.begin_prompt(prompt, false),
            Some(_) => {
                self.notice = Some("Queued local action is not runnable as a prompt".to_string());
                Vec::new()
            }
            None => self.begin_prompt(next.prompt, false),
        }
    }

    fn cancel(&mut self) -> Vec<CodeEffect> {
        if self.turn == TurnState::Cancelling {
            return Vec::new();
        }
        if self.turn == TurnState::Ready {
            self.dispatch_after_permissions = false;
            self.permission_selected = None;
            let effects = self
                .permissions
                .drain(..)
                .map(|pending| CodeEffect::ResolvePermission {
                    id: pending.prompt.id().to_string(),
                    outcome: RequestPermissionOutcome::Cancelled,
                })
                .collect::<Vec<_>>();
            self.restore_after_permission();
            self.notice =
                Some("Turn completed; pending permissions cancelled. Queue is paused.".to_string());
            self.refresh_open_palettes();
            return effects;
        }
        self.turn = TurnState::Cancelling;
        self.dispatch_after_permissions = false;
        self.surface = Surface::Conversation;
        self.permission_selected = None;
        self.permission_return = None;
        let mut effects = self
            .permissions
            .drain(..)
            .map(|pending| CodeEffect::ResolvePermission {
                id: pending.prompt.id().to_string(),
                outcome: RequestPermissionOutcome::Cancelled,
            })
            .collect::<Vec<_>>();
        effects.push(CodeEffect::Cancel);
        self.refresh_open_palettes();
        effects
    }

    fn clear_disconnected_permissions(&mut self) {
        self.permissions.clear();
        self.permission_selected = None;
        let return_to = self.permission_return.take();
        if matches!(&self.surface, Surface::Permission)
            || matches!(
                &self.surface,
                Surface::Inspector(OpenInspector {
                    return_to_permission: true,
                    ..
                })
            )
        {
            self.surface = return_to.map_or(Surface::Conversation, |surface| surface);
        } else if let Surface::Inspector(inspector) = &mut self.surface
            && inspector
                .return_to
                .as_deref()
                .is_some_and(surface_returns_to_permission)
        {
            inspector.return_to = return_to.map(Box::new);
        }
    }

    fn open_palette(&mut self, slash: bool, query: String) {
        let return_to = self.transient_return_target();
        self.open_palette_returning_to(slash, query, return_to);
    }

    fn open_palette_returning_to(
        &mut self,
        slash: bool,
        query: String,
        return_to: Option<Box<Surface>>,
    ) {
        let choices = self
            .palette_commands()
            .into_iter()
            .map(Choice::Command)
            .collect::<Vec<_>>();
        let mut list = ChoiceList {
            kind: ChoiceListKind::Palette { slash },
            title: if slash {
                "Slash commands".to_string()
            } else {
                "Commands".to_string()
            },
            detail: "Arrow keys move · Enter accepts · Esc restores draft".to_string(),
            query,
            choices,
            matches: Vec::new(),
            selected: None,
            allow_custom: false,
            custom_label: String::new(),
            return_to,
        };
        list.filter();
        self.surface = Surface::Palette(list);
    }

    fn transient_return_target(&self) -> Option<Box<Surface>> {
        (!matches!(self.surface, Surface::Conversation)).then(|| Box::new(self.surface.clone()))
    }

    fn close_choice(&mut self) {
        let return_to = match &self.surface {
            Surface::Palette(list) | Surface::Selector(list) => list.return_to.clone(),
            _ => None,
        };
        self.surface = return_to.map_or(Surface::Conversation, |surface| *surface);
    }

    fn effective_activity(&self) -> String {
        if self.turn == TurnState::Cancelling {
            return "cancelling".to_string();
        }
        if !self.permissions.is_empty() {
            return "permission needed".to_string();
        }
        match self.turn {
            TurnState::Submitting | TurnState::Working => "working".to_string(),
            TurnState::Ready => self.status.activity.clone(),
            TurnState::Cancelling => "cancelling".to_string(),
        }
    }

    fn palette_commands(&self) -> Vec<Command> {
        let mut commands = self
            .commands
            .iter()
            .filter(|command| {
                !self.operations_only
                    || !matches!(
                        &command.target,
                        CommandTarget::AgentPrompt { .. } | CommandTarget::PromptTemplate { .. }
                    )
            })
            .cloned()
            .map(|command| self.command_with_local_availability(command))
            .collect::<Vec<_>>();
        if !self.operations_only && self.journal.commands_received() {
            commands.extend(self.journal.commands().iter().map(|command| {
                let label = if command.name.starts_with('/') {
                    command.name.clone()
                } else {
                    format!("/{}", command.name)
                };
                Command::new(
                    label.clone(),
                    command.description.clone(),
                    CommandOwner::Agent,
                    CommandTarget::AgentPrompt { prompt: label },
                )
            }));
        } else if !self.operations_only {
            commands.push(
                Command::new(
                    "Agent commands",
                    "Waiting for the active agent to advertise commands",
                    CommandOwner::Agent,
                    CommandTarget::AgentPrompt {
                        prompt: String::new(),
                    },
                )
                .unavailable("Agent command list has not arrived"),
            );
        }
        commands
    }

    fn refresh_open_palettes(&mut self) {
        let choices = self
            .palette_commands()
            .into_iter()
            .map(Choice::Command)
            .collect::<Vec<_>>();
        refresh_palette_surface(&mut self.surface, &choices);
        if let Some(surface) = &mut self.permission_return {
            refresh_palette_surface(surface, &choices);
        }
    }

    fn command_with_local_availability(&self, mut command: Command) -> Command {
        if command.unavailable.is_some() {
            return command;
        }
        let unavailable = match &command.target {
            CommandTarget::ChooseAgent => self.session_replacement_reason(),
            CommandTarget::OpenSession => self.session_replacement_reason().or_else(|| {
                (!self.session_active)
                    .then_some("Choose an agent before opening a native session".to_string())
            }),
            CommandTarget::Settings => self.session_replacement_reason().or_else(|| {
                (!self.session_active)
                    .then_some("Choose an agent before opening agent settings".to_string())
                    .or_else(|| {
                        (!self
                            .selectors
                            .iter()
                            .any(|selector| selector.id == "settings"))
                        .then_some(
                            "This session has not reported editable agent settings".to_string(),
                        )
                    })
            }),
            _ => None,
        };
        if let Some(reason) = unavailable {
            command.unavailable = Some(reason);
        }
        command
    }

    fn session_replacement_reason(&self) -> Option<String> {
        if self.operations_only {
            return Some("Remote operations are read-only".to_string());
        }
        if !self.permissions.is_empty() {
            return Some("Resolve pending permissions before changing this session".to_string());
        }
        if self.turn != TurnState::Ready {
            return Some(
                "Finish or cancel the current turn before changing this session".to_string(),
            );
        }
        if !self.queue.is_empty() {
            return Some(
                "Resolve or discard queued prompts before changing this session".to_string(),
            );
        }
        None
    }

    fn agent_command_available(&self, prompt: &str) -> bool {
        let head = prompt.split_whitespace().next().unwrap_or_default();
        self.journal.commands().iter().any(|command| {
            if command.name.starts_with('/') {
                head == command.name
            } else {
                head == format!("/{}", command.name)
            }
        })
    }

    fn clear_stale_command(&mut self) {
        if self
            .selected_command
            .as_ref()
            .is_some_and(|(text, _, _)| text != self.editor.text())
        {
            self.selected_command = None;
        }
    }

    fn read_previous(&mut self) {
        if self.reading_positions.is_empty() {
            return;
        }
        let current = self
            .reading_anchor
            .as_ref()
            .and_then(|anchor| self.reading_position(anchor))
            .unwrap_or(self.reading_positions.len());
        let target = current.saturating_sub(self.reading_page);
        if let Some(position) = self.reading_positions.get(target).cloned() {
            self.follow_live = false;
            self.reading_anchor = Some(ReadingAnchor {
                entry: position.entry,
                source_offset: position.source_offset,
            });
        }
    }

    fn read_next(&mut self) {
        let Some(anchor) = self.reading_anchor.clone() else {
            return;
        };
        let Some(position) = self.reading_position(&anchor) else {
            self.return_to_live();
            return;
        };
        if let Some(next) = self
            .reading_positions
            .get(position.saturating_add(self.reading_page))
            .cloned()
        {
            self.reading_anchor = Some(ReadingAnchor {
                entry: next.entry,
                source_offset: next.source_offset,
            });
        } else {
            self.return_to_live();
        }
    }

    fn reading_position(&self, anchor: &ReadingAnchor) -> Option<usize> {
        self.reading_positions
            .iter()
            .position(|position| {
                position.entry == anchor.entry && position.source_offset >= anchor.source_offset
            })
            .or_else(|| {
                self.reading_positions
                    .iter()
                    .rposition(|position| position.entry == anchor.entry)
            })
    }

    fn return_to_live(&mut self) {
        self.follow_live = true;
        self.reading_anchor = None;
        self.new_activity = false;
    }

    fn finish_journal_stream(&mut self) {
        self.journal.finish_stream();
        self.journal_revision = self.journal_revision.saturating_add(1);
    }

    fn inspect_anchor(&mut self) {
        let id = self
            .reading_anchor
            .as_ref()
            .map(|anchor| anchor.entry.clone())
            .or_else(|| self.journal.entries().last().map(|item| item.id));
        let Some(id) = id else {
            self.notice = Some("There is no transcript entry to inspect".to_string());
            return;
        };
        let Some(inspector) = self.inspector_for(&id) else {
            self.notice = Some("This transcript entry has no inspectable content".to_string());
            return;
        };
        self.open_inspector(inspector);
    }

    fn inspect_permission_context(&mut self) {
        let context = self
            .permissions
            .front()
            .and_then(|pending| pending.context.as_ref());
        let Some(context) = context else {
            self.notice = Some("The agent supplied no structured permission context".to_string());
            return;
        };
        let content = match serde_json::to_string_pretty(context) {
            Ok(content) => content,
            Err(error) => format!("Could not serialise permission context: {error}"),
        };
        self.surface = Surface::Inspector(OpenInspector {
            inspector: Inspector::new("Permission context", content),
            scroll: 0,
            search: String::new(),
            searching: false,
            return_to: None,
            return_to_permission: true,
        });
    }

    fn copy_anchor(&self) -> Vec<CodeEffect> {
        let id = self
            .reading_anchor
            .as_ref()
            .map(|anchor| anchor.entry.clone())
            .or_else(|| self.journal.entries().last().map(|item| item.id));
        let Some(id) = id else {
            return Vec::new();
        };
        let Some(inspector) = self.inspector_for(&id) else {
            return Vec::new();
        };
        vec![CodeEffect::Copy {
            text: inspector.content,
        }]
    }

    fn inspector_for(&self, target: &EntryId) -> Option<Inspector> {
        self.journal.entries().find_map(|item| {
            if item.id != *target {
                return None;
            }
            let inspector = match item.entry {
                Entry::Message(message) => Inspector::new("Message", message.text.clone()),
                Entry::Tool(call) => {
                    let content = match serde_json::to_string_pretty(call) {
                        Ok(content) => content,
                        Err(error) => format!("Could not serialise retained tool content: {error}"),
                    };
                    Inspector::new(format!("Tool · {}", call.title), content)
                }
                Entry::Plan(plan) => {
                    let content = match serde_json::to_string_pretty(plan) {
                        Ok(content) => content,
                        Err(error) => format!("Could not serialise retained plan: {error}"),
                    };
                    Inspector::new("Plan", content)
                }
            };
            Some(inspector)
        })
    }

    fn search_transcript(&mut self, query: &str) {
        if query.is_empty() {
            return;
        }
        let needle = query.to_lowercase();
        let ids = self
            .journal
            .entries()
            .map(|item| item.id)
            .collect::<Vec<_>>();
        for id in ids {
            let Some(inspector) = self.inspector_for(&id) else {
                continue;
            };
            let haystack = inspector.content.to_lowercase();
            let Some(offset) = haystack.find(&needle) else {
                continue;
            };
            self.follow_live = false;
            self.reading_anchor = Some(ReadingAnchor {
                entry: id,
                source_offset: inspector.content[..offset].graphemes(true).count(),
            });
            return;
        }
    }
}

impl ChoiceList {
    fn selector(
        selector: &Selector,
        choices: Vec<Choice>,
        return_to: Option<Box<Surface>>,
    ) -> Self {
        let mut list = Self {
            kind: ChoiceListKind::Selector {
                id: selector.id.clone(),
            },
            title: selector.title.clone(),
            detail: selector.detail.clone(),
            query: String::new(),
            choices,
            matches: Vec::new(),
            selected: None,
            allow_custom: selector.allow_custom,
            custom_label: selector.custom_label.clone(),
            return_to,
        };
        list.filter();
        list
    }

    fn filter(&mut self) {
        let query = self.query.to_lowercase();
        self.matches = self
            .choices
            .iter()
            .enumerate()
            .filter(|(_, choice)| choice.searchable().to_lowercase().contains(&query))
            .map(|(index, _)| index)
            .collect();
        self.selected = (!self.matches.is_empty()).then_some(0);
    }

    fn replace_choices(&mut self, choices: Vec<Choice>) {
        let selected = self
            .selected
            .and_then(|selected| self.matches.get(selected))
            .and_then(|index| self.choices.get(*index))
            .cloned();
        self.choices = choices;
        self.filter();
        if let Some(selected) = selected {
            self.selected = self.matches.iter().position(|index| {
                self.choices
                    .get(*index)
                    .is_some_and(|choice| choice.same_identity(&selected))
            });
        }
    }

    fn move_by(&mut self, delta: isize) {
        let Some(selected) = self.selected else {
            return;
        };
        self.selected = Some(
            selected
                .saturating_add_signed(delta)
                .min(self.matches.len().saturating_sub(1)),
        );
    }

    fn first(&mut self) {
        self.selected = (!self.matches.is_empty()).then_some(0);
    }

    fn last(&mut self) {
        self.selected = (!self.matches.is_empty()).then_some(self.matches.len().saturating_sub(1));
    }

    fn accepted(&self) -> Option<Accepted> {
        if let Some(index) = self
            .selected
            .and_then(|selected| self.matches.get(selected))
        {
            return self
                .choices
                .get(*index)
                .cloned()
                .map(|choice| Accepted::Choice(choice, self.kind.clone()));
        }
        let ChoiceListKind::Selector { id } = &self.kind else {
            return None;
        };
        if self.allow_custom && !self.query.trim().is_empty() {
            return Some(Accepted::Custom {
                selector: id.clone(),
                value: self.query.clone(),
            });
        }
        None
    }

    fn sync_slash_draft(&self, editor: &mut Editor) {
        if matches!(self.kind, ChoiceListKind::Palette { slash: true }) {
            editor.set_text(format!("/{}", self.query));
        }
    }
}

fn refresh_palette_surface(surface: &mut Surface, choices: &[Choice]) {
    if let Surface::Palette(list) = surface {
        list.replace_choices(choices.to_vec());
    }
}

fn refresh_selector_surface(surface: &mut Surface, selectors: &[Selector]) -> bool {
    let Surface::Selector(list) = surface else {
        return true;
    };
    if refresh_selector_list(list, selectors) {
        return true;
    }
    *surface = Surface::Conversation;
    false
}

fn refresh_selector_list(list: &mut ChoiceList, selectors: &[Selector]) -> bool {
    let ChoiceListKind::Selector { id } = &list.kind else {
        return true;
    };
    let id = id.clone();
    let Some(selector) = selectors.iter().find(|selector| selector.id == id) else {
        return false;
    };
    list.title = selector.title.clone();
    list.detail = selector.detail.clone();
    list.allow_custom = selector.allow_custom;
    list.custom_label = selector.custom_label.clone();
    list.replace_choices(
        selector
            .rows
            .iter()
            .cloned()
            .map(Choice::Selector)
            .collect(),
    );
    true
}

fn is_async_mutation_selector(selector: &str) -> bool {
    selector == "route" || selector == "mode" || selector.starts_with("config:")
}

fn surface_returns_to_permission(surface: &Surface) -> bool {
    match surface {
        Surface::Permission => true,
        Surface::Inspector(inspector) => {
            inspector.return_to_permission
                || inspector
                    .return_to
                    .as_deref()
                    .is_some_and(surface_returns_to_permission)
        }
        Surface::Palette(list) | Surface::Selector(list) => list
            .return_to
            .as_deref()
            .is_some_and(surface_returns_to_permission),
        Surface::Conversation | Surface::Queue { .. } | Surface::TranscriptSearch { .. } => false,
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Accepted {
    Choice(Choice, ChoiceListKind),
    Custom { selector: String, value: String },
}

impl Choice {
    fn searchable(&self) -> String {
        match self {
            Self::Command(command) => format!(
                "{} {} {} {}",
                command.label,
                command.detail,
                command.owner.label(),
                command.unavailable.as_deref().unwrap_or_default()
            ),
            Self::Selector(row) => format!(
                "{} {} {}",
                row.label,
                row.detail,
                row.unavailable.as_deref().unwrap_or_default()
            ),
        }
    }

    fn label(&self) -> &str {
        match self {
            Self::Command(command) => &command.label,
            Self::Selector(row) => &row.label,
        }
    }

    fn detail(&self) -> String {
        match self {
            Self::Command(command) => {
                let unavailable = command
                    .unavailable
                    .as_ref()
                    .map(|reason| format!(" · unavailable: {reason}"))
                    .unwrap_or_default();
                format!(
                    "{} · {}{unavailable}",
                    command.owner.label(),
                    command.detail
                )
            }
            Self::Selector(row) => {
                let unavailable = row
                    .unavailable
                    .as_ref()
                    .map(|reason| format!(" · unavailable: {reason}"))
                    .unwrap_or_default();
                format!("{}{}", row.detail, unavailable)
            }
        }
    }

    fn same_identity(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Command(left), Self::Command(right)) => {
                left.label == right.label
                    && left.owner == right.owner
                    && left.target == right.target
            }
            (Self::Selector(left), Self::Selector(right)) => left.id == right.id,
            _ => false,
        }
    }
}

/// Full-screen terminal custody for Code state.
///
/// The app can own this type without naming a terminal frame or any renderer
/// type from the terminal library.
pub struct CodeView {
    terminal: Terminal<CrosstermBackend<std::io::Stdout>>,
    registry: Registry,
    document: DocumentCache,
    finished: bool,
    suspended: bool,
}

impl CodeView {
    /// Enter full-screen raw mode.
    pub fn open() -> io::Result<Self> {
        if !std::io::stdin().is_terminal() || !std::io::stdout().is_terminal() {
            return Err(io::Error::other(
                "bitrouter code requires an interactive stdin and stdout",
            ));
        }
        crate::lifecycle::install_panic_restore();
        crate::lifecycle::enter_raw()?;
        let mut stdout = std::io::stdout();
        if let Err(error) = crate::lifecycle::enter_alternate_screen()
            .and_then(|()| crate::lifecycle::enable_session_keys())
            .and_then(|()| execute!(stdout, Hide))
        {
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
            registry: Registry::default(),
            document: DocumentCache::default(),
            finished: false,
            suspended: false,
        })
    }

    /// Draw one full frame from pure Code state.
    pub fn draw(&mut self, state: &mut CodeState) -> io::Result<()> {
        if self.finished || self.suspended {
            return Ok(());
        }
        let registry = &self.registry;
        let document = &mut self.document;
        self.terminal
            .draw(|frame| render_frame(frame, state, registry, document))
            .map(|_| ())
    }

    /// Restore terminal settings before launching an external editor.
    pub fn suspend(&mut self) -> io::Result<()> {
        if !self.finished && !self.suspended {
            crate::lifecycle::restore();
            self.suspended = true;
        }
        Ok(())
    }

    /// Restore the full-screen surface after an external editor exits.
    pub fn resume(&mut self) -> io::Result<()> {
        if self.finished || !self.suspended {
            return Ok(());
        }
        crate::lifecycle::enter_raw()?;
        if let Err(error) = crate::lifecycle::enter_alternate_screen()
            .and_then(|()| crate::lifecycle::enable_session_keys())
            .and_then(|()| execute!(self.terminal.backend_mut(), Hide))
        {
            crate::lifecycle::restore();
            return Err(error);
        }
        self.terminal.clear()?;
        self.suspended = false;
        Ok(())
    }

    /// Restore the user's terminal. Calling it repeatedly is harmless.
    pub fn finish(&mut self) -> io::Result<()> {
        if !self.finished {
            crate::lifecycle::restore();
            self.finished = true;
        }
        Ok(())
    }
}

impl Drop for CodeView {
    fn drop(&mut self) {
        let _ = self.finish();
    }
}

#[derive(Clone)]
struct DocumentRow {
    source_offset: usize,
    line: Line<'static>,
}

#[derive(Default)]
struct DocumentCache {
    width: u16,
    height: u16,
    source_revision: u64,
    total_rows: usize,
    entries: Vec<CachedDocumentEntry>,
    positions: Vec<ReadingPosition>,
    layout_revision: u64,
}

struct CachedDocumentEntry {
    id: EntryId,
    revision: u64,
    start: usize,
    rows: Vec<DocumentRow>,
}

impl DocumentCache {
    fn refresh(&mut self, state: &CodeState, width: u16, height: u16, registry: &Registry) {
        let geometry_changed = self.width != width || self.height != height;
        if !geometry_changed && self.source_revision == state.journal_revision {
            return;
        }
        let mut changed = geometry_changed;
        let mut previous = HashMap::with_capacity(self.entries.len());
        if !geometry_changed {
            for entry in std::mem::take(&mut self.entries) {
                previous.insert(entry.id.clone(), entry);
            }
        } else {
            self.entries.clear();
        }

        let mut entries = Vec::new();
        let mut start = 0_usize;
        for item in state.journal.entries() {
            let cached = previous
                .remove(&item.id)
                .filter(|cached| cached.revision == item.revision);
            let mut entry = match cached {
                Some(entry) => entry,
                None => {
                    changed = true;
                    CachedDocumentEntry {
                        id: item.id.clone(),
                        revision: item.revision,
                        start: 0,
                        rows: document_rows_for_entry(item.entry, width, height, registry),
                    }
                }
            };
            if entry.revision != item.revision {
                changed = true;
            }
            entry.start = start;
            start = start.saturating_add(entry.rows.len());
            entries.push(entry);
        }
        if !previous.is_empty() {
            changed = true;
        }
        self.width = width;
        self.height = height;
        self.source_revision = state.journal_revision;
        self.total_rows = start;
        self.entries = entries;
        if changed {
            self.positions = self
                .entries
                .iter()
                .flat_map(|entry| {
                    entry.rows.iter().map(move |row| ReadingPosition {
                        entry: entry.id.clone(),
                        source_offset: row.source_offset,
                    })
                })
                .collect();
            self.layout_revision = self.layout_revision.saturating_add(1);
        }
    }

    fn start_for(&self, state: &CodeState, visible: usize) -> usize {
        if state.follow_live {
            return self.total_rows.saturating_sub(visible);
        }
        let Some(anchor) = &state.reading_anchor else {
            return self.total_rows.saturating_sub(visible);
        };
        self.entries
            .iter()
            .find(|entry| entry.id == anchor.entry)
            .and_then(|entry| {
                entry
                    .rows
                    .iter()
                    .position(|row| row.source_offset >= anchor.source_offset)
                    .map(|offset| entry.start.saturating_add(offset))
            })
            .unwrap_or_else(|| self.total_rows.saturating_sub(visible))
    }

    fn visible_lines(&self, start: usize, count: usize) -> Vec<Line<'static>> {
        let end = start.saturating_add(count);
        let mut lines = Vec::with_capacity(count);
        for entry in &self.entries {
            let entry_end = entry.start.saturating_add(entry.rows.len());
            if entry_end <= start {
                continue;
            }
            if entry.start >= end {
                break;
            }
            let local_start = start.saturating_sub(entry.start);
            let local_end = end.saturating_sub(entry.start).min(entry.rows.len());
            lines.extend(
                entry.rows[local_start..local_end]
                    .iter()
                    .map(|row| row.line.clone()),
            );
        }
        lines
    }
}

fn render_frame(
    frame: &mut Frame<'_>,
    state: &mut CodeState,
    registry: &Registry,
    document: &mut DocumentCache,
) {
    let area = frame.area();
    if area.width < 40 || area.height < 16 {
        frame.render_widget(
            Paragraph::new(
                "Resize terminal to at least 40×16. Conversation state and draft are retained.",
            )
            .style(Style::default().fg(Color::Yellow))
            .wrap(Wrap { trim: true }),
            area,
        );
        return;
    }

    if state.operations_only {
        render_operations_base(frame, area, state);
        render_surface(frame, state);
        return;
    }

    let status_height = if area.width < 68 { 4 } else { 2 };
    let notice_height = u16::from(state.notice.is_some()) * 2;
    let queue_height = if state.queue.is_empty() {
        0
    } else {
        u16::try_from(state.queue.len().min(2))
            .unwrap_or(u16::MAX)
            .saturating_add(2)
    };
    let composer_height = composer_height(state, area.width);
    let [
        header,
        status,
        transcript,
        queue,
        notice_area,
        composer,
        hint_area,
    ] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Length(status_height),
        Constraint::Min(3),
        Constraint::Length(queue_height),
        Constraint::Length(notice_height),
        Constraint::Length(composer_height),
        Constraint::Length(1),
    ])
    .areas(area);

    frame.render_widget(
        Paragraph::new(Line::styled(
            state.status.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        header,
    );
    render_status(frame, status, state);
    render_transcript(frame, transcript, state, registry, document);
    if !state.queue.is_empty() {
        render_queue_summary(frame, queue, state);
    }
    if let Some(notice) = &state.notice {
        frame.render_widget(
            Paragraph::new(notice.as_str())
                .style(Style::default().fg(Color::Yellow))
                .block(Block::default().borders(Borders::TOP).title(" Notice "))
                .wrap(Wrap { trim: true }),
            notice_area,
        );
    }
    render_composer(frame, composer, state);
    frame.render_widget(
        Paragraph::new(hint(state)).style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );

    render_surface(frame, state);

    if matches!(state.surface, Surface::Conversation) {
        set_composer_cursor(frame, composer, state);
    }
}

fn render_operations_base(frame: &mut Frame<'_>, area: Rect, state: &CodeState) {
    let [header, body, hint_area] = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(1),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            state.status.title.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        )),
        header,
    );
    frame.render_widget(
        Paragraph::new("Read-only operations\nLoading target status…")
            .style(Style::default().fg(Color::DarkGray)),
        body,
    );
    frame.render_widget(
        Paragraph::new("Ctrl-P commands · Esc close").style(Style::default().fg(Color::DarkGray)),
        hint_area,
    );
}

fn render_surface(frame: &mut Frame<'_>, state: &CodeState) {
    match &state.surface {
        Surface::Conversation => {}
        Surface::Palette(list) | Surface::Selector(list) => render_choice_list(frame, list),
        Surface::Inspector(inspector) => render_inspector(frame, inspector),
        Surface::Permission => render_permission(frame, state),
        Surface::Queue { selected } => render_queue_editor(frame, state, *selected),
        Surface::TranscriptSearch { query } => render_transcript_search(frame, query),
    }
}

fn render_status(frame: &mut Frame<'_>, area: Rect, state: &CodeState) {
    let full_cost = match state.journal.usage().and_then(cost::from_usage) {
        Some(cost) => line_text(&cost.render()),
        None => line_text(&cost::unreported()),
    };
    let width = usize::from(area.width);
    let activity = state.effective_activity();
    let lines = if area.width < 68 {
        let cost = compact_cost(&full_cost, width.saturating_sub("session cost: ".width()));
        vec![
            Line::from(format!(
                "agent: {}",
                truncate_cells(&state.status.agent, width.saturating_sub("agent: ".width()))
            )),
            Line::from(format!(
                "route: {}",
                truncate_cells(&state.status.route, width.saturating_sub("route: ".width()))
            )),
            Line::from(format!(
                "activity: {}",
                truncate_cells(&activity, width.saturating_sub("activity: ".width()))
            )),
            Line::from(format!(
                "session cost: {}",
                truncate_cells(&cost, width.saturating_sub("session cost: ".width()))
            )),
        ]
    } else {
        let first_fixed = "agent: ".width().saturating_add(" · route: ".width());
        let first_available = width.saturating_sub(first_fixed);
        let agent_width = first_available.saturating_mul(45).saturating_div(100);
        let route_width = first_available.saturating_sub(agent_width);
        let second_labels = "activity: "
            .width()
            .saturating_add(" · session cost: ".width());
        let second_available = width.saturating_sub(second_labels);
        let cost = compact_cost(&full_cost, second_available.saturating_div(2));
        let activity_width = second_available.saturating_sub(cost.width());
        vec![
            Line::from(format!(
                "agent: {} · route: {}",
                truncate_cells(&state.status.agent, agent_width),
                truncate_cells(&state.status.route, route_width)
            )),
            Line::from(format!(
                "activity: {} · session cost: {cost}",
                truncate_cells(&activity, activity_width)
            )),
        ]
    };
    frame.render_widget(Paragraph::new(lines), area);
}

fn compact_cost(cost: &str, cells: usize) -> String {
    if cost.width() <= cells {
        return cost.to_string();
    }
    let (source, figure) = if let Some(figure) = cost.strip_suffix(" (router)") {
        ("router", figure)
    } else if let Some(figure) = cost.strip_prefix("agent-reported ") {
        ("agent-reported", figure)
    } else {
        return truncate_cells(cost, cells);
    };
    let source_width = source.width();
    if cells <= source_width {
        return truncate_cells(source, cells);
    }
    let separator = " · ";
    let remaining = cells.saturating_sub(source_width.saturating_add(separator.width()));
    if remaining == 0 {
        return truncate_cells(source, cells);
    }
    format!("{source}{separator}{}", truncate_cells(figure, remaining))
}

fn render_transcript(
    frame: &mut Frame<'_>,
    area: Rect,
    state: &mut CodeState,
    registry: &Registry,
    document: &mut DocumentCache,
) {
    document.refresh(state, area.width.saturating_sub(2), area.height, registry);
    let visible = usize::from(area.height.saturating_sub(2));
    state.sync_reading_layout(document.layout_revision, &document.positions, visible);
    let start = document.start_for(state, visible);
    let lines = document.visible_lines(start, visible);
    let title = if state.follow_live {
        " Conversation "
    } else if state.new_activity {
        " Reading history · new activity "
    } else {
        " Reading history "
    };
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(title)),
        area,
    );
}

fn document_rows_for_entry(
    entry: Entry<'_>,
    width: u16,
    height: u16,
    registry: &Registry,
) -> Vec<DocumentRow> {
    let mut rows = Vec::new();
    let (lines, source_offsets) = match entry {
        Entry::Message(message) if message.voice == Voice::Agent => {
            let lines = render::markdown::source_lines(&message.text);
            let offsets = source_line_offsets(&message.text, lines.len());
            (lines, offsets)
        }
        Entry::Message(message) => {
            let lines = render::source_message(message);
            let offsets = source_line_offsets(&message.text, lines.len());
            (lines, offsets)
        }
        Entry::Tool(call) => {
            let lines = compact_tool_lines(call, width, height, registry);
            let offsets = (0..lines.len()).collect();
            (lines, offsets)
        }
        Entry::Plan(plan) => {
            let lines = render::session::plan(plan);
            let offsets = (0..lines.len()).collect();
            (lines, offsets)
        }
    };
    for (line_index, line) in lines.into_iter().enumerate() {
        let mut source_offset = source_offsets.get(line_index).copied().unwrap_or_default();
        for line in wrap(&sanitize_line(&line), width.max(1)) {
            let source_length = line_graphemes(&line);
            rows.push(DocumentRow {
                source_offset,
                line,
            });
            source_offset = source_offset.saturating_add(source_length);
        }
    }
    rows
}

fn compact_tool_lines(
    call: &ToolCall,
    width: u16,
    height: u16,
    registry: &Registry,
) -> Vec<Line<'static>> {
    let mut lines = registry.render(&ToolContext::new(
        call,
        ratatui::layout::Size::new(width, height),
    ));
    let limit = match call.status {
        ToolCallStatus::Completed => 4,
        ToolCallStatus::Failed => 8,
        _ => 6,
    };
    if lines.len() > limit {
        let hidden = lines.len().saturating_sub(limit);
        lines.truncate(limit);
        let summary = match call.status {
            ToolCallStatus::Completed => {
                format!("  [{hidden} more rows · F4 inspects full tool output]")
            }
            ToolCallStatus::Failed => {
                format!("  [{hidden} more diagnostic rows · F4 inspects full tool output]")
            }
            _ => format!("  [{hidden} more rows · F4 inspects full tool output]"),
        };
        lines.push(Line::styled(summary, Style::default().fg(Color::DarkGray)));
    }
    lines
}

fn source_line_offsets(text: &str, count: usize) -> Vec<usize> {
    let mut offsets = Vec::new();
    let mut offset = 0_usize;
    for line in text.split('\n') {
        offsets.push(offset);
        offset = offset
            .saturating_add(line.graphemes(true).count())
            .saturating_add(1);
    }
    if offsets.is_empty() {
        offsets.push(0);
    }
    let fallback = offsets.last().copied().unwrap_or_default();
    while offsets.len() < count {
        offsets.push(fallback);
    }
    offsets.truncate(count);
    offsets
}

fn line_graphemes(line: &Line<'_>) -> usize {
    line.spans
        .iter()
        .map(|span| span.content.graphemes(true).count())
        .sum()
}

fn render_queue_summary(frame: &mut Frame<'_>, area: Rect, state: &CodeState) {
    let lines = state
        .queue
        .iter()
        .take(2)
        .enumerate()
        .map(|(index, prompt)| {
            let owner = prompt.owner.map(CommandOwner::label).unwrap_or("prompt");
            Line::from(format!(
                "{}: {} [{}]",
                index.saturating_add(1),
                one_line(&prompt.prompt),
                owner
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines)
            .block(
                Block::default()
                    .borders(Borders::TOP)
                    .title(" Next turn · F3 edit/remove "),
            )
            .wrap(Wrap { trim: true }),
        area,
    );
}

fn composer_height(state: &CodeState, width: u16) -> u16 {
    let layout = composer_layout(
        state.editor.text(),
        state.editor.cursor_byte(),
        width.saturating_sub(4).max(1),
    );
    let rows = layout.rows.len().clamp(1, 4);
    u16::try_from(rows).unwrap_or(u16::MAX).saturating_add(2)
}

fn render_composer(frame: &mut Frame<'_>, area: Rect, state: &CodeState) {
    let inner = Block::default()
        .borders(Borders::ALL)
        .title(" Composer ")
        .inner(area);
    let layout = composer_layout(
        state.editor.text(),
        state.editor.cursor_byte(),
        inner.width.saturating_sub(2).max(1),
    );
    let start = layout
        .cursor_row
        .saturating_sub(usize::from(inner.height.saturating_sub(1)));
    let lines = layout
        .rows
        .iter()
        .skip(start)
        .take(usize::from(inner.height))
        .cloned()
        .collect::<Vec<_>>();
    frame.render_widget(
        Paragraph::new(lines).block(Block::default().borders(Borders::ALL).title(" Composer ")),
        area,
    );
}

fn set_composer_cursor(frame: &mut Frame<'_>, area: Rect, state: &CodeState) {
    let inner = Block::default()
        .borders(Borders::ALL)
        .title(" Composer ")
        .inner(area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let layout = composer_layout(
        state.editor.text(),
        state.editor.cursor_byte(),
        inner.width.saturating_sub(2).max(1),
    );
    let start = layout
        .cursor_row
        .saturating_sub(usize::from(inner.height.saturating_sub(1)));
    let y_offset = u16::try_from(layout.cursor_row.saturating_sub(start)).unwrap_or(u16::MAX);
    let x = inner
        .x
        .saturating_add(layout.cursor_column)
        .min(inner.right().saturating_sub(1));
    let y = inner
        .y
        .saturating_add(y_offset)
        .min(inner.bottom().saturating_sub(1));
    frame.set_cursor_position(Position::new(x, y));
}

#[derive(Clone)]
struct ComposerLayout {
    rows: Vec<Line<'static>>,
    cursor_row: usize,
    cursor_column: u16,
}

fn composer_layout(text: &str, cursor: usize, content_width: u16) -> ComposerLayout {
    let mut rows = Vec::new();
    let mut cursor_position = None;
    let mut start = 0_usize;
    let bytes = text.as_bytes();
    let mut index = 0_usize;

    while index < bytes.len() {
        let newline = matches!(bytes[index], b'\n' | b'\r');
        if !newline {
            index = index.saturating_add(1);
            continue;
        }
        append_composer_line(
            &mut rows,
            &mut cursor_position,
            &text[start..index],
            start,
            cursor,
            content_width,
        );
        let mut next = index.saturating_add(1);
        if bytes[index] == b'\r' && bytes.get(next) == Some(&b'\n') {
            next = next.saturating_add(1);
        }
        if cursor > index && cursor < next {
            cursor_position = rows.last().map(|line| {
                (
                    rows.len().saturating_sub(1),
                    u16::try_from(line_width(line)).unwrap_or(u16::MAX),
                )
            });
        }
        start = next;
        index = next;
    }
    append_composer_line(
        &mut rows,
        &mut cursor_position,
        &text[start..],
        start,
        cursor,
        content_width,
    );
    let (cursor_row, cursor_column) = cursor_position.unwrap_or_else(|| {
        let row = rows.len().saturating_sub(1);
        let column = rows
            .last()
            .map(|line| u16::try_from(line_width(line)).unwrap_or(u16::MAX))
            .unwrap_or(2);
        (row, column)
    });
    ComposerLayout {
        rows,
        cursor_row,
        cursor_column,
    }
}

fn append_composer_line(
    rows: &mut Vec<Line<'static>>,
    cursor_position: &mut Option<(usize, u16)>,
    text: &str,
    start: usize,
    cursor: usize,
    content_width: u16,
) {
    let mut content = String::new();
    let mut cells = 0_u16;
    let width = content_width.max(1);
    let mut first_row = true;
    let mut row_start = start;

    for (offset, grapheme) in text.grapheme_indices(true) {
        let position = start.saturating_add(offset);
        if cursor_position.is_none() && cursor == position {
            *cursor_position = Some((
                rows.len(),
                u16::try_from(2_usize.saturating_add(usize::from(cells))).unwrap_or(u16::MAX),
            ));
        }
        let rendered = sanitize(grapheme);
        let grapheme_cells = u16::try_from(rendered.width()).unwrap_or(u16::MAX);
        if !content.is_empty() && cells.saturating_add(grapheme_cells) > width {
            rows.push(composer_row(std::mem::take(&mut content), first_row));
            first_row = false;
            row_start = position;
            cells = 0;
        }
        if cursor_position.is_none() && cursor == row_start {
            *cursor_position = Some((rows.len(), 2));
        }
        content.push_str(&rendered);
        cells = cells.saturating_add(grapheme_cells);
    }
    let end = start.saturating_add(text.len());
    if cursor_position.is_none() && cursor == end {
        *cursor_position = Some((
            rows.len(),
            u16::try_from(2_usize.saturating_add(usize::from(cells))).unwrap_or(u16::MAX),
        ));
    }
    rows.push(composer_row(content, first_row));
}

fn composer_row(content: String, first: bool) -> Line<'static> {
    let prefix = if first { "› " } else { "  " };
    Line::from(vec![
        Span::styled(prefix, Style::default().fg(Color::Cyan)),
        Span::raw(content),
    ])
}

fn line_width(line: &Line<'_>) -> usize {
    line.spans.iter().map(|span| span.content.width()).sum()
}

fn hint(state: &CodeState) -> String {
    if state.operations_only {
        return "Ctrl-P commands · Esc close".to_string();
    }
    match state.turn {
        TurnState::Submitting | TurnState::Working => {
            if state.permissions.is_empty() {
                "Tab queue next · Esc interrupt · Ctrl-P commands · F3 queue".to_string()
            } else {
                format!(
                    "F2 permission ({}) · Tab queue next · Esc interrupt · Ctrl-P commands",
                    state.permissions.len()
                )
            }
        }
        TurnState::Cancelling => "Cancelling · wait for the agent to settle".to_string(),
        TurnState::Ready => {
            if state.permissions.is_empty() {
                "Ctrl-P commands · F2 permissions · F4 inspect · Ctrl-F search · Ctrl-End live"
                    .to_string()
            } else {
                format!(
                    "F2 permission ({}) · Ctrl-P commands · F4 inspect · Ctrl-End live",
                    state.permissions.len()
                )
            }
        }
    }
}

fn render_choice_list(frame: &mut Frame<'_>, list: &ChoiceList) {
    let area = centered(frame.area(), 86, 70, 12, 20);
    frame.render_widget(Clear, area);
    let [heading, query, choices, footer] = Layout::vertical([
        Constraint::Length(2),
        Constraint::Length(3),
        Constraint::Min(4),
        Constraint::Length(2),
    ])
    .areas(area);
    frame.render_widget(
        Paragraph::new(Line::styled(
            safe_one_line(&list.detail),
            Style::default().fg(Color::DarkGray),
        ))
        .block(
            Block::default()
                .borders(Borders::ALL)
                .title(safe_one_line(&list.title)),
        ),
        heading,
    );
    let query_title = if list.query.is_empty() {
        " Search · type to filter "
    } else {
        " Search "
    };
    frame.render_widget(
        Paragraph::new(safe_one_line(&list.query))
            .block(Block::default().borders(Borders::ALL).title(query_title)),
        query,
    );

    let results_block = Block::default().borders(Borders::ALL).title(" Results ");
    let results = results_block.inner(choices);
    frame.render_widget(results_block, choices);
    let mut lines = Vec::new();
    let mut selected_start = None;
    let mut selected_end = None;
    for (visible, index) in list.matches.iter().enumerate() {
        let Some(choice) = list.choices.get(*index) else {
            continue;
        };
        let selected = list.selected == Some(visible);
        let marker = if selected { "› " } else { "  " };
        let style = if selected {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let start = lines.len();
        let label = Line::from(vec![Span::styled(
            format!("{marker}{}", safe_one_line(choice.label())),
            style,
        )]);
        lines.extend(wrap(&sanitize_line(&label), results.width.max(1)));
        let detail = Line::styled(
            format!("    {}", safe_one_line(&choice.detail())),
            Style::default().fg(Color::DarkGray),
        );
        lines.extend(wrap(&detail, results.width.max(1)));
        if selected {
            selected_start = Some(start);
            selected_end = Some(lines.len());
        }
    }
    if lines.is_empty() {
        lines.push(Line::from("No matches"));
        if list.allow_custom && !list.query.trim().is_empty() {
            lines.push(Line::from(format!(
                "Enter confirms custom value: {}",
                safe_one_line(&list.custom_label)
            )));
        }
    }
    let visible = usize::from(results.height);
    let selected_start = selected_start.unwrap_or_default();
    let selected_end = selected_end.unwrap_or(selected_start.saturating_add(1));
    let centered = selected_start.saturating_sub(visible.saturating_div(2));
    let mut start = centered.min(lines.len().saturating_sub(visible));
    if selected_end.saturating_sub(start) > visible {
        start = selected_start;
    }
    let end = start.saturating_add(visible).min(lines.len());
    frame.render_widget(
        Paragraph::new(lines[start..end].to_vec()).wrap(Wrap { trim: false }),
        results,
    );
    frame.render_widget(
        Paragraph::new("↑/↓ move · Enter select · Esc/Ctrl-C close · digits filter")
            .style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

fn render_inspector(frame: &mut Frame<'_>, inspector: &OpenInspector) {
    let area = centered(frame.area(), 96, 90, 14, 22);
    frame.render_widget(Clear, area);
    let title = if inspector.searching {
        format!(
            " {} · search: {} ",
            inspector.inspector.title, inspector.search
        )
    } else if inspector.search.is_empty() {
        format!(" {} ", inspector.inspector.title)
    } else {
        format!(
            " {} · {} matches ",
            inspector.inspector.title,
            match_count(&inspector.inspector.content, &inspector.search)
        )
    };
    frame.render_widget(
        Paragraph::new(
            inspector
                .inspector
                .content
                .lines()
                .map(|line| Line::from(sanitize(line)))
                .collect::<Vec<_>>(),
        )
        .block(Block::default().borders(Borders::ALL).title(title))
        .scroll((u16::try_from(inspector.scroll).unwrap_or(u16::MAX), 0))
        .wrap(Wrap { trim: false }),
        area,
    );
    let footer = Rect::new(
        area.x.saturating_add(1),
        area.bottom().saturating_sub(1),
        area.width.saturating_sub(2),
        1,
    );
    frame.render_widget(
        Paragraph::new("Ctrl-P commands · Ctrl-F search · Ctrl-Y copy · Esc/Ctrl-C close")
            .style(Style::default().fg(Color::DarkGray)),
        footer,
    );
}

fn render_permission(frame: &mut Frame<'_>, state: &CodeState) {
    let viewport = frame.area();
    let area = if viewport.width <= 48 || viewport.height <= 18 {
        viewport
    } else {
        centered(viewport, 86, 66, 10, 16)
    };
    frame.render_widget(Clear, area);
    let Some(pending) = state.permissions.front() else {
        return;
    };
    let prompt = &pending.prompt;
    let block = Block::default().borders(Borders::ALL).title(" Permission ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    if inner.width == 0 || inner.height == 0 {
        return;
    }
    let width = usize::from(inner.width);
    let mut header = vec![Line::styled(
        truncate_cells(&safe_one_line(prompt.title()), width),
        Style::default().add_modifier(Modifier::BOLD),
    )];
    header.push(Line::from(truncate_cells(
        &format!(
            "request {} · pending {}",
            safe_one_line(prompt.id()),
            state.permissions.len()
        ),
        width,
    )));
    if let Some(context) = &pending.context {
        header.push(Line::styled(
            truncate_cells(&safe_one_line(&permission_context_summary(context)), width),
            Style::default().fg(Color::DarkGray),
        ));
        header.push(Line::styled(
            truncate_cells(
                "F4 inspects complete command, diff, and location context",
                width,
            ),
            Style::default().fg(Color::DarkGray),
        ));
    }
    let footer = vec![
        Line::styled(
            "Press a number to highlight",
            Style::default().fg(Color::DarkGray),
        ),
        Line::styled(
            "Enter confirms selection · Esc rejects/cancels",
            Style::default().fg(Color::DarkGray),
        ),
    ];
    let header_height = u16::try_from(header.len()).unwrap_or(u16::MAX);
    let footer_height = u16::try_from(footer.len()).unwrap_or(u16::MAX);
    let [header_area, options_area, footer_area] = Layout::vertical([
        Constraint::Length(header_height.min(inner.height.saturating_sub(footer_height))),
        Constraint::Min(1),
        Constraint::Length(footer_height.min(inner.height)),
    ])
    .areas(inner);
    frame.render_widget(Paragraph::new(header), header_area);

    let mut options = Vec::new();
    let mut selected_start = None;
    let mut selected_end = None;
    for (index, option) in prompt.options().iter().enumerate() {
        let selected = state.permission_selected == Some(index);
        let marker = if selected { "›" } else { " " };
        let style = if selected {
            Style::default()
                .fg(Color::Yellow)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default()
        };
        let start = options.len();
        options.extend(wrap(
            &Line::from(Span::styled(
                format!(
                    "{marker} [{}] {}",
                    index.saturating_add(1),
                    safe_one_line(&option.name)
                ),
                style,
            )),
            options_area.width.max(1),
        ));
        if selected {
            selected_start = Some(start);
            selected_end = Some(options.len());
        }
    }
    let visible = usize::from(options_area.height);
    let selected_start = selected_start.unwrap_or_default();
    let selected_end = selected_end.unwrap_or(selected_start.saturating_add(1));
    let centered = selected_start.saturating_sub(visible.saturating_div(2));
    let mut start = centered.min(options.len().saturating_sub(visible));
    if selected_end.saturating_sub(start) > visible {
        start = selected_start;
    }
    let end = start.saturating_add(visible).min(options.len());
    frame.render_widget(
        Paragraph::new(options[start..end].to_vec()).wrap(Wrap { trim: false }),
        options_area,
    );
    frame.render_widget(Paragraph::new(footer), footer_area);
}

fn permission_context_summary(context: &ToolCallUpdate) -> String {
    let kind = context
        .fields
        .kind
        .map(|kind| format!("{kind:?}"))
        .unwrap_or_else(|| "unreported kind".to_string());
    let locations = context.fields.locations.as_ref().map_or(0, Vec::len);
    let diffs = context.fields.content.as_ref().map_or(0, |content| {
        content
            .iter()
            .filter(|item| {
                matches!(
                    item,
                    agent_client_protocol_schema::v1::ToolCallContent::Diff(_)
                )
            })
            .count()
    });
    let raw = usize::from(context.fields.raw_input.is_some())
        .saturating_add(usize::from(context.fields.raw_output.is_some()));
    format!(
        "tool {} · kind {kind} · {diffs} diff(s) · {locations} location(s) · {raw} raw payload(s)",
        context.tool_call_id
    )
}

fn render_queue_editor(frame: &mut Frame<'_>, state: &CodeState, selected: usize) {
    let area = centered(frame.area(), 82, 60, 8, 14);
    frame.render_widget(Clear, area);
    let block = Block::default()
        .borders(Borders::ALL)
        .title(" Next-turn queue ");
    let inner = block.inner(area);
    frame.render_widget(block, area);
    let [rows, footer] = Layout::vertical([Constraint::Min(1), Constraint::Length(2)]).areas(inner);
    let visible = usize::from(rows.height);
    let start = selected
        .saturating_sub(visible / 2)
        .min(state.queue.len().saturating_sub(visible));
    let lines = state
        .queue
        .iter()
        .enumerate()
        .skip(start)
        .take(visible)
        .map(|(index, item)| {
            let marker = if index == selected { "› " } else { "  " };
            let style = if index == selected {
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD)
            } else {
                Style::default()
            };
            Line::from(Span::styled(
                truncate_cells(
                    &format!("{marker}{}", safe_one_line(&item.prompt)),
                    usize::from(rows.width),
                ),
                style,
            ))
        })
        .collect::<Vec<_>>();
    frame.render_widget(Paragraph::new(lines), rows);
    frame.render_widget(
        Paragraph::new("↑/↓ select · Enter/e edit · Delete remove · Esc close")
            .style(Style::default().fg(Color::DarkGray))
            .wrap(Wrap { trim: true }),
        footer,
    );
}

fn render_transcript_search(frame: &mut Frame<'_>, query: &str) {
    let area = centered(frame.area(), 75, 50, 5, 7);
    frame.render_widget(Clear, area);
    frame.render_widget(
        Paragraph::new(vec![
            Line::from("Search transcript"),
            Line::from(query.to_string()),
            Line::styled(
                "Enter keeps match · Esc closes",
                Style::default().fg(Color::DarkGray),
            ),
        ])
        .block(Block::default().borders(Borders::ALL)),
        area,
    );
}

fn centered(
    area: Rect,
    width_percent: u16,
    height_percent: u16,
    min_width: u16,
    min_height: u16,
) -> Rect {
    let width = area
        .width
        .saturating_mul(width_percent)
        .saturating_div(100)
        .max(min_width)
        .min(area.width);
    let height = area
        .height
        .saturating_mul(height_percent)
        .saturating_div(100)
        .max(min_height)
        .min(area.height);
    Rect::new(
        area.x.saturating_add(area.width.saturating_sub(width) / 2),
        area.y
            .saturating_add(area.height.saturating_sub(height) / 2),
        width,
        height,
    )
}

fn sanitize_line(line: &Line<'_>) -> Line<'static> {
    Line::from(
        line.spans
            .iter()
            .map(|span| Span::styled(sanitize(span.content.as_ref()), span.style))
            .collect::<Vec<_>>(),
    )
}

fn sanitize(text: &str) -> String {
    text.replace('\u{1b}', "␛")
        .replace('\r', "")
        .replace('\t', "    ")
}

fn one_line(text: &str) -> String {
    text.lines()
        .next()
        .map_or_else(String::new, ToString::to_string)
}

fn safe_one_line(text: &str) -> String {
    one_line(&sanitize(text))
}

fn line_text(line: &Line<'_>) -> String {
    line.spans
        .iter()
        .map(|span| span.content.as_ref())
        .collect::<String>()
}

fn first_match_line(content: &str, query: &str) -> Option<usize> {
    if query.is_empty() {
        return Some(0);
    }
    let query = query.to_lowercase();
    content
        .lines()
        .position(|line| line.to_lowercase().contains(&query))
}

fn match_count(content: &str, query: &str) -> usize {
    if query.is_empty() {
        return 0;
    }
    content
        .to_lowercase()
        .matches(&query.to_lowercase())
        .count()
}

fn truncate_cells(text: &str, width: usize) -> String {
    if text.width() <= width {
        return text.to_string();
    }
    if width == 0 {
        return String::new();
    }
    let mut out = String::new();
    let mut cells: usize = 0;
    let budget = width.saturating_sub(1);
    for grapheme in text.graphemes(true) {
        let next = grapheme.width();
        if cells.saturating_add(next) > budget {
            break;
        }
        out.push_str(grapheme);
        cells = cells.saturating_add(next);
    }
    out.push('…');
    out
}

fn pressed(event: &Event) -> Option<&KeyEvent> {
    match event {
        Event::Key(key) if is_press(key) => Some(key),
        _ => None,
    }
}

fn is_press(key: &KeyEvent) -> bool {
    matches!(key.kind, KeyEventKind::Press | KeyEventKind::Repeat)
}

fn control(key: &KeyEvent, character: char) -> bool {
    key.code == KeyCode::Char(character) && key.modifiers.contains(KeyModifiers::CONTROL)
}

fn previous_selection(selected: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(selected.unwrap_or(0).saturating_sub(1))
    }
}

fn next_selection(selected: Option<usize>, len: usize) -> Option<usize> {
    if len == 0 {
        None
    } else {
        Some(
            selected
                .unwrap_or(0)
                .saturating_add(1)
                .min(len.saturating_sub(1)),
        )
    }
}

#[cfg(test)]
mod tests {
    use std::io;

    use agent_client_protocol_schema::v1::{
        AvailableCommand, AvailableCommandsUpdate, ContentBlock, ContentChunk, Cost as WireCost,
        Diff, MessageId, PermissionOption, PermissionOptionId, PermissionOptionKind, TextContent,
        ToolCall, ToolCallContent, ToolCallId, ToolCallStatus, ToolCallUpdateFields, UsageUpdate,
    };
    use crossterm::event::{Event, KeyEvent};
    use ratatui::backend::TestBackend;

    use super::*;

    fn press(code: KeyCode) -> CodeAction {
        CodeAction::Event(Event::Key(KeyEvent::new(code, KeyModifiers::NONE)))
    }

    fn ctrl(character: char) -> CodeAction {
        CodeAction::Event(Event::Key(KeyEvent::new(
            KeyCode::Char(character),
            KeyModifiers::CONTROL,
        )))
    }

    fn paste(text: &str) -> CodeAction {
        CodeAction::Event(Event::Paste(text.to_string()))
    }

    fn option(id: &str, kind: PermissionOptionKind) -> PermissionOption {
        PermissionOption::new(PermissionOptionId::new(id), id, kind)
    }

    fn question(id: &str) -> Prompt {
        Prompt::new(
            id,
            Some("Write src/main.rs".to_string()),
            "tool-1",
            None,
            vec![
                option("allow", PermissionOptionKind::AllowOnce),
                option("reject-always", PermissionOptionKind::RejectAlways),
                option("reject-once", PermissionOptionKind::RejectOnce),
            ],
        )
    }

    fn active_state() -> CodeState {
        let mut state = CodeState::default();
        state.set_session_active(true);
        state
    }

    fn grid(backend: &TestBackend) -> String {
        let buffer = backend.buffer();
        (0..buffer.area.height)
            .map(|y| {
                (0..buffer.area.width)
                    .map(|x| buffer[(x, y)].symbol())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    fn start_working(state: &mut CodeState, prompt: &str) {
        let _ = state.step(paste(prompt));
        let effects = state.step(press(KeyCode::Enter));
        assert!(matches!(&effects[..], [CodeEffect::Submit { prompt: sent }] if sent == prompt));
        let _ = state.step(CodeAction::TurnStarted);
    }

    #[test]
    fn permission_requires_explicit_highlight_and_prefers_reject_once() {
        let mut state = active_state();
        assert!(
            state
                .receive_permission(question("permission-1"))
                .is_empty()
        );

        assert!(state.step(press(KeyCode::F(2))).is_empty());
        assert!(state.step(press(KeyCode::Enter)).is_empty());
        assert!(
            state
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Select a permission option"))
        );

        assert!(state.step(press(KeyCode::Char('1'))).is_empty());
        let selected = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &selected[..],
            [CodeEffect::ResolvePermission {
                id,
                outcome: RequestPermissionOutcome::Selected(choice),
            }] if id == "permission-1" && choice.option_id.0.as_ref() == "allow"
        ));

        assert!(
            state
                .receive_permission(question("permission-2"))
                .is_empty()
        );
        let _ = state.step(press(KeyCode::F(2)));
        let rejected = state.step(press(KeyCode::Esc));
        assert!(matches!(
            &rejected[..],
            [CodeEffect::ResolvePermission {
                outcome: RequestPermissionOutcome::Selected(choice),
                ..
            }] if choice.option_id.0.as_ref() == "reject-once"
        ));
    }

    #[test]
    fn modified_enter_edits_a_follow_up_without_submitting_or_queueing() {
        let mut state = active_state();
        start_working(&mut state, "first prompt");
        let _ = state.step(paste("follow up"));
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            assert!(
                state
                    .step(CodeAction::Event(Event::Key(KeyEvent::new(
                        KeyCode::Enter,
                        modifiers,
                    ))))
                    .is_empty()
            );
        }
        assert_eq!(state.editor().text(), "follow up\n\n");
        assert_eq!(state.queue_len(), 0);
        assert_eq!(state.turn, TurnState::Working);
    }

    #[test]
    fn completion_waits_for_the_last_permission_before_dispatching_queue() {
        let mut state = active_state();
        start_working(&mut state, "first prompt");
        let _ = state.step(paste("follow up"));
        let _ = state.step(press(KeyCode::Tab));
        assert_eq!(state.queue_len(), 1);
        assert!(
            state
                .receive_permission(question("permission-1"))
                .is_empty()
        );

        assert!(
            state
                .step(CodeAction::TurnSettled(TurnOutcome::Completed))
                .is_empty()
        );
        assert_eq!(state.queue_len(), 1);
        let _ = state.step(press(KeyCode::F(2)));
        let _ = state.step(press(KeyCode::Char('1')));
        let effects = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &effects[..],
            [
                CodeEffect::ResolvePermission { .. },
                CodeEffect::Submit { prompt },
            ] if prompt == "follow up"
        ));
        assert_eq!(state.queue_len(), 0);
    }

    #[test]
    fn cancelling_late_permissions_keeps_an_ended_turn_ready_and_pauses_follow_ups() {
        let mut state = active_state();
        state.set_status(CodeStatus {
            activity: "ready".to_string(),
            ..CodeStatus::default()
        });
        start_working(&mut state, "first prompt");
        let _ = state.step(paste("follow up"));
        let _ = state.step(press(KeyCode::Tab));
        assert!(state.receive_permission(question("late-one")).is_empty());
        assert!(state.receive_permission(question("late-two")).is_empty());
        assert!(
            state
                .step(CodeAction::TurnSettled(TurnOutcome::Completed))
                .is_empty()
        );

        let _ = state.step(press(KeyCode::F(2)));
        let cancelled = state.step(ctrl('c'));
        let cancelled_ids = cancelled
            .iter()
            .filter_map(|effect| match effect {
                CodeEffect::ResolvePermission {
                    id,
                    outcome: RequestPermissionOutcome::Cancelled,
                } => Some(id.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(cancelled_ids, vec!["late-one", "late-two"]);
        assert!(
            !cancelled
                .iter()
                .any(|effect| matches!(effect, CodeEffect::Cancel))
        );
        assert_eq!(state.queue_len(), 1);
        assert_eq!(state.effective_activity(), "ready");
        assert!(matches!(state.surface, Surface::Conversation));
        assert!(
            state
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("Queue is paused"))
        );
    }

    #[test]
    fn queued_local_commands_are_refused_templates_expand_and_agent_commands_revalidate() {
        let mut state = active_state();
        state.set_typed_commands(
            vec![crate::machine::Command {
                name: "route",
                action: "route_set",
                summary: "change route",
                unavailable: None,
            }],
            vec![crate::machine::PromptCommand {
                name: "review".to_string(),
                description: "review the change".to_string(),
                template: "Review $ARGUMENTS".to_string(),
            }],
        );
        start_working(&mut state, "first prompt");

        let _ = state.step(paste("/route fast"));
        let _ = state.step(press(KeyCode::Tab));
        assert_eq!(state.queue_len(), 0);
        assert!(
            state
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("cannot be queued"))
        );

        let _ = state.step(CodeAction::ExternalEditorFinished(Ok(
            "/review narrow layout".to_string(),
        )));
        let _ = state.step(press(KeyCode::Tab));
        assert_eq!(state.queue_len(), 1);
        let next = state.step(CodeAction::TurnSettled(TurnOutcome::Completed));
        assert!(matches!(
            &next[..],
            [CodeEffect::Submit { prompt }] if prompt == "Review narrow layout"
        ));

        let mut agent_state = active_state();
        start_working(&mut agent_state, "first prompt");
        agent_state.apply(SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(vec![AvailableCommand::new(
                "deploy",
                "deploy the preview",
            )]),
        ));
        let _ = agent_state.step(paste("/deploy preview"));
        let _ = agent_state.step(press(KeyCode::Tab));
        assert_eq!(agent_state.queue_len(), 1);
        agent_state.apply(SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(Vec::new()),
        ));
        assert!(
            agent_state
                .step(CodeAction::TurnSettled(TurnOutcome::Completed))
                .is_empty()
        );
        assert_eq!(agent_state.queue_len(), 1);
        assert!(
            agent_state
                .notice
                .as_deref()
                .is_some_and(|notice| notice.contains("no longer advertised"))
        );
    }

    #[test]
    fn local_commands_run_without_a_session_while_prompts_open_the_agent_chooser() {
        let mut state = CodeState::default();
        state.set_typed_commands(
            vec![crate::machine::Command {
                name: "status",
                action: "status",
                summary: "show target status",
                unavailable: None,
            }],
            Vec::new(),
        );
        let _ = state.step(paste("/status"));
        let local = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &local[..],
            [CodeEffect::LocalAction { action, args }]
                if action == "status" && args.is_empty()
        ));
        assert_eq!(state.editor().text(), "/status");

        let mut prompt = CodeState::default();
        let _ = prompt.step(paste("ordinary draft before connection"));
        let chooser = prompt.step(press(KeyCode::Enter));
        assert!(matches!(&chooser[..], [CodeEffect::ChooseAgent]));
        assert_eq!(prompt.editor().text(), "ordinary draft before connection");
        assert_eq!(prompt.effective_activity(), "choosing agent");
    }

    #[test]
    fn session_controls_refresh_to_the_current_turn_and_permission_state() {
        let mut state = active_state();
        state.set_commands(vec![
            Command::new(
                "Choose agent",
                "Open an ACP agent",
                CommandOwner::BitRouter,
                CommandTarget::ChooseAgent,
            ),
            Command::new(
                "Open session",
                "Load a native session",
                CommandOwner::BitRouter,
                CommandTarget::OpenSession,
            ),
            Command::new(
                "Agent settings",
                "Settings reported by the agent",
                CommandOwner::BitRouter,
                CommandTarget::Settings,
            ),
        ]);
        state.set_selectors(vec![Selector::new(
            "settings",
            "Agent settings",
            "",
            Vec::new(),
        )]);
        start_working(&mut state, "busy prompt");
        let _ = state.step(ctrl('p'));
        let working_reasons = match &state.surface {
            Surface::Palette(list) => list
                .choices
                .iter()
                .filter_map(|choice| match choice {
                    Choice::Command(command) if command.owner == CommandOwner::BitRouter => {
                        command.unavailable.as_deref()
                    }
                    Choice::Command(_) | Choice::Selector(_) => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        assert_eq!(working_reasons.len(), 3);
        assert!(
            working_reasons
                .iter()
                .all(|reason| reason.contains("current turn"))
        );

        assert!(
            state
                .receive_permission(question("control-permission"))
                .is_empty()
        );
        let permission_reasons = match &state.surface {
            Surface::Palette(list) => list
                .choices
                .iter()
                .filter_map(|choice| match choice {
                    Choice::Command(command) if command.owner == CommandOwner::BitRouter => {
                        command.unavailable.as_deref()
                    }
                    Choice::Command(_) | Choice::Selector(_) => None,
                })
                .collect::<Vec<_>>(),
            _ => Vec::new(),
        };
        assert_eq!(permission_reasons.len(), 3);
        assert!(
            permission_reasons
                .iter()
                .all(|reason| reason.contains("pending permissions"))
        );
    }

    #[test]
    fn agent_command_selection_survives_submission_rejection() -> io::Result<()> {
        let mut state = active_state();
        state.set_commands(vec![Command::new(
            "/route",
            "change the BitRouter route",
            CommandOwner::BitRouter,
            CommandTarget::LocalAction {
                action: "route_set".to_string(),
                args: Vec::new(),
            },
        )]);
        state.apply(SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(vec![AvailableCommand::new(
                "route",
                "the agent route command",
            )]),
        ));

        let _ = state.step(ctrl('p'));
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let palette = grid(terminal.backend());
        assert!(palette.contains("BitRouter"));
        assert!(palette.contains("Agent"));
        let _ = state.step(press(KeyCode::Down));
        let sent = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &sent[..],
            [CodeEffect::AgentPrompt { prompt }] if prompt == "/route"
        ));
        let _ = state.step(CodeAction::SubmissionRejected {
            prompt: "/route".to_string(),
            reason: "connection still opening".to_string(),
        });
        assert_eq!(state.editor().text(), "/route");
        let retry = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &retry[..],
            [CodeEffect::AgentPrompt { prompt }] if prompt == "/route"
        ));
        Ok(())
    }

    #[test]
    fn selectors_preserve_explicit_custom_values() {
        let mut state = active_state();
        state.set_selectors(vec![
            Selector::new(
                "native-session",
                "Open native session",
                "Enter an opaque native id",
                vec![SelectorRow::new(
                    "listed",
                    "Listed session",
                    "from this page",
                )],
            )
            .allow_custom("Native session id"),
        ]);
        assert!(state.open_selector("native-session"));
        let _ = state.step(press(KeyCode::Char('x')));
        let effects = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &effects[..],
            [CodeEffect::Select { selector, id, custom: true }]
                if selector == "native-session" && id == "x"
        ));
    }

    #[test]
    fn reading_anchor_and_full_tool_inspection_survive_new_activity() {
        let mut state = active_state();
        let long = "one long retained line ".repeat(20);
        state.apply(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(long)))
                .message_id(MessageId::new("m1")),
        ));
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        cache.refresh(&state, 16, 10, &registry);
        state.sync_reading_layout(cache.layout_revision, &cache.positions, 8);
        let _ = state.step(press(KeyCode::PageUp));
        let anchor = state.reading_anchor.clone();
        assert!(anchor.is_some());

        state.apply(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new("new live activity")))
                .message_id(MessageId::new("m2")),
        ));
        cache.refresh(&state, 16, 10, &registry);
        state.sync_reading_layout(cache.layout_revision, &cache.positions, 8);
        assert_eq!(state.reading_anchor, anchor);
        assert!(state.new_activity);

        state.return_to_live();
        state.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("tool-1"), "Read complete output")
                .status(ToolCallStatus::Completed)
                .content(vec![ToolCallContent::from(ContentBlock::Text(
                    TextContent::new("full retained tool output including the final diff hunk"),
                ))]),
        ));
        let _ = state.step(press(KeyCode::F(4)));
        let inspector = match &state.surface {
            Surface::Inspector(inspector) => inspector,
            _ => {
                assert!(
                    matches!(&state.surface, Surface::Inspector(_)),
                    "F4 should open the full retained tool inspector"
                );
                return;
            }
        };
        assert!(inspector.inspector.content.contains("final diff hunk"));
        let copied = state.step(ctrl('y'));
        assert!(matches!(
            &copied[..],
            [CodeEffect::Copy { text }] if text.contains("final diff hunk")
        ));
    }

    #[test]
    fn status_activity_follows_the_local_turn_and_permission_lifecycle() -> io::Result<()> {
        let mut state = CodeState::new(CodeStatus {
            title: "project".to_string(),
            agent: "fixture-agent".to_string(),
            route: "default (no override)".to_string(),
            activity: "ready".to_string(),
        });
        state.set_session_active(true);
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;

        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert!(grid(terminal.backend()).contains("activity: ready"));

        start_working(&mut state, "work through the status lifecycle");
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert!(grid(terminal.backend()).contains("activity: working"));

        assert!(
            state
                .receive_permission(question("status-permission"))
                .is_empty()
        );
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert!(grid(terminal.backend()).contains("activity: permission needed"));

        let _ = state.step(press(KeyCode::F(2)));
        let cancelled = state.step(ctrl('c'));
        assert!(
            cancelled
                .iter()
                .any(|effect| matches!(effect, CodeEffect::Cancel))
        );
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert!(grid(terminal.backend()).contains("activity: cancelling"));

        assert!(
            state
                .step(CodeAction::TurnSettled(TurnOutcome::Completed))
                .is_empty()
        );
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert!(grid(terminal.backend()).contains("activity: ready"));

        let mut disconnected = state.status().clone();
        disconnected.activity = "disconnected · adapter closed".to_string();
        state.set_status(disconnected);
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert!(grid(terminal.backend()).contains("activity: disconnected · adapter closed"));
        Ok(())
    }

    #[test]
    fn completion_after_cancellation_reports_end_turn_but_pauses_the_queue() {
        let mut state = active_state();
        start_working(&mut state, "first prompt");
        let _ = state.step(paste("follow up only after normal completion"));
        let _ = state.step(press(KeyCode::Tab));
        assert_eq!(state.queue_len(), 1);

        let cancelled = state.step(press(KeyCode::Esc));
        assert!(
            cancelled
                .iter()
                .any(|effect| matches!(effect, CodeEffect::Cancel))
        );
        assert!(
            state
                .step(CodeAction::TurnSettled(TurnOutcome::Completed))
                .is_empty()
        );
        assert_eq!(state.queue_len(), 1);
        assert!(state.notice.as_deref().is_some_and(|notice| {
            notice.contains("completed after cancellation request")
                && notice.contains("Queue is paused")
        }));
    }

    #[test]
    fn accepted_prompts_are_local_user_entries_and_rejections_are_not() {
        let mut state = active_state();
        let _ = state.step(paste("accepted exactly once"));
        let sent = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &sent[..],
            [CodeEffect::Submit { prompt }] if prompt == "accepted exactly once"
        ));
        assert_eq!(state.journal().entries().count(), 0);
        let _ = state.step(CodeAction::TurnStarted);
        let user_entries = state
            .journal()
            .entries()
            .filter_map(|item| match item.entry {
                Entry::Message(message) if message.voice == Voice::User => {
                    Some(message.text.clone())
                }
                Entry::Message(_) | Entry::Tool(_) | Entry::Plan(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(user_entries, vec!["accepted exactly once".to_string()]);

        state.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("turn-tool"), "Read a file")
                .status(ToolCallStatus::Completed),
        ));
        state.apply(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("first turn response")),
        )));
        let _ = state.step(CodeAction::TurnSettled(TurnOutcome::Completed));

        let _ = state.step(paste("second exact prompt"));
        let _ = state.step(press(KeyCode::Enter));
        let _ = state.step(CodeAction::TurnStarted);
        state.apply(SessionUpdate::AgentMessageChunk(ContentChunk::new(
            ContentBlock::Text(TextContent::new("second turn response")),
        )));

        let messages = state
            .journal()
            .entries()
            .filter_map(|item| match item.entry {
                Entry::Message(message) => {
                    Some((message.voice, message.text.clone(), message.complete))
                }
                Entry::Tool(_) | Entry::Plan(_) => None,
            })
            .collect::<Vec<_>>();
        assert_eq!(
            messages,
            vec![
                (Voice::User, "accepted exactly once".to_string(), true),
                (Voice::Agent, "first turn response".to_string(), true),
                (Voice::User, "second exact prompt".to_string(), true),
                (Voice::Agent, "second turn response".to_string(), false),
            ]
        );

        let mut rejected = active_state();
        let _ = rejected.step(paste("must not appear"));
        let _ = rejected.step(press(KeyCode::Enter));
        let _ = rejected.step(CodeAction::SubmissionRejected {
            prompt: "must not appear".to_string(),
            reason: "connection refused".to_string(),
        });
        assert_eq!(rejected.journal().entries().count(), 0);
        assert_eq!(rejected.editor().text(), "must not appear");
    }

    #[test]
    fn disconnect_clears_unreachable_permissions_and_restores_the_prior_inspector() {
        let mut state = active_state();
        state.open_inspector(Inspector::new("Session details", "retained session data"));
        let context = ToolCallUpdate::new(
            ToolCallId::new("pending-tool"),
            ToolCallUpdateFields::default(),
        );
        assert!(
            state
                .receive_permission_with_context(question("permission-one"), context)
                .is_empty()
        );
        assert!(
            state
                .receive_permission(question("permission-two"))
                .is_empty()
        );
        state.queue.push_back(QueuedPrompt {
            prompt: "keep paused after disconnect".to_string(),
            owner: None,
            target: None,
        });
        let _ = state.step(press(KeyCode::F(2)));
        let _ = state.step(press(KeyCode::F(4)));
        assert!(matches!(state.surface, Surface::Inspector(_)));
        let _ = state.step(press(KeyCode::F(2)));
        assert!(matches!(state.surface, Surface::Permission));
        let _ = state.step(press(KeyCode::F(4)));
        assert!(matches!(state.surface, Surface::Inspector(_)));

        let effects = state.step(CodeAction::TurnSettled(TurnOutcome::Disconnected));
        assert!(effects.is_empty());
        assert!(state.permissions.is_empty());
        assert!(state.permission_selected.is_none());
        assert_eq!(state.queue_len(), 1);
        assert!(matches!(
            &state.surface,
            Surface::Inspector(inspector) if inspector.inspector.title == "Session details"
        ));
    }

    #[test]
    fn disconnect_reparents_an_error_inspector_that_arrived_during_permission_focus() {
        let mut state = active_state();
        state.open_inspector(Inspector::new("Session details", "retained session data"));
        assert!(
            state
                .receive_permission(question("permission-one"))
                .is_empty()
        );
        let _ = state.step(press(KeyCode::F(2)));
        state.open_inspector(Inspector::new(
            "Operation failed",
            "report failed while waiting",
        ));
        assert!(matches!(
            &state.surface,
            Surface::Inspector(inspector) if inspector.inspector.title == "Operation failed"
        ));

        assert!(
            state
                .step(CodeAction::TurnSettled(TurnOutcome::Disconnected))
                .is_empty()
        );
        assert!(state.permissions.is_empty());
        assert!(state.step(press(KeyCode::Esc)).is_empty());
        assert!(matches!(
            &state.surface,
            Surface::Inspector(inspector) if inspector.inspector.title == "Session details"
        ));
    }

    #[test]
    fn ctrl_c_closes_transient_surfaces_without_losing_the_draft_or_anchor() {
        let mut state = active_state();
        for index in 0..8 {
            state.apply(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(format!(
                    "retained transcript row {index}"
                ))))
                .message_id(MessageId::new(format!("modal-{index}"))),
            ));
        }
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        cache.refresh(&state, 12, 8, &registry);
        state.sync_reading_layout(cache.layout_revision, &cache.positions, 6);
        let _ = state.step(press(KeyCode::PageUp));
        let anchor = state.reading_anchor.clone();
        assert!(anchor.is_some());

        let _ = state.step(paste("draft with a grapheme ���\nsecond line"));
        let _ = state.step(press(KeyCode::Left));
        let draft = state.editor().text().to_string();
        let cursor = state.editor().cursor_byte();

        state.open_inspector(Inspector::new("Details", "retained inspector output"));
        state.apply(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(
                "still consuming updates",
            )))
            .message_id(MessageId::new("modal-update")),
        ));
        assert!(state.step(ctrl('c')).is_empty());
        assert!(matches!(state.surface, Surface::Conversation));
        assert_eq!(state.editor().text(), draft);
        assert_eq!(state.editor().cursor_byte(), cursor);
        assert_eq!(state.reading_anchor, anchor);

        let _ = state.step(ctrl('p'));
        assert!(matches!(state.surface, Surface::Palette(_)));
        assert!(state.step(ctrl('c')).is_empty());
        assert!(matches!(state.surface, Surface::Conversation));

        state.set_selectors(vec![Selector::new(
            "native-session",
            "Open native session",
            "A temporary picker",
            vec![SelectorRow::new(
                "listed",
                "Listed session",
                "from the agent",
            )],
        )]);
        assert!(state.open_selector("native-session"));
        assert!(state.step(ctrl('c')).is_empty());
        assert!(matches!(state.surface, Surface::Conversation));

        let _ = state.step(ctrl('f'));
        assert!(matches!(state.surface, Surface::TranscriptSearch { .. }));
        assert!(state.step(ctrl('c')).is_empty());
        assert!(matches!(state.surface, Surface::Conversation));

        let context = ToolCallUpdate::new(
            ToolCallId::new("permission-tool"),
            ToolCallUpdateFields::default(),
        );
        assert!(
            state
                .receive_permission_with_context(question("permission-context"), context)
                .is_empty()
        );
        let _ = state.step(press(KeyCode::F(2)));
        let _ = state.step(press(KeyCode::F(4)));
        assert!(matches!(state.surface, Surface::Inspector(_)));
        assert!(state.step(ctrl('c')).is_empty());
        assert!(matches!(state.surface, Surface::Permission));
        assert_eq!(state.editor().text(), draft);
        assert_eq!(state.editor().cursor_byte(), cursor);
        assert_eq!(state.reading_anchor, anchor);

        let mut operations = CodeState::default();
        operations.set_operations_only(true);
        operations.set_commands(vec![Command::new(
            "Status",
            "Target status",
            CommandOwner::BitRouter,
            CommandTarget::Report {
                id: "status".to_string(),
            },
        )]);
        operations.operations_root(Inspector::new("Target status", "remote status"));
        let _ = operations.step(ctrl('p'));
        let agent_rows = match &operations.surface {
            Surface::Palette(list) => list
                .choices
                .iter()
                .filter(|choice| matches!(choice, Choice::Command(command) if command.owner == CommandOwner::Agent))
                .count(),
            _ => 1,
        };
        assert_eq!(agent_rows, 0);
        assert!(operations.step(ctrl('c')).is_empty());
        assert!(matches!(
            &operations.surface,
            Surface::Inspector(inspector) if inspector.inspector.title == "Target status"
        ));
        assert!(matches!(
            &operations.step(ctrl('c'))[..],
            [CodeEffect::Exit]
        ));
    }

    #[test]
    fn inspector_restores_the_picker_that_was_open_when_a_report_arrived() {
        let mut state = active_state();
        let _ = state.step(paste("draft survives report errors"));
        let _ = state.step(press(KeyCode::Left));
        let draft = state.editor().text().to_string();
        let cursor = state.editor().cursor_byte();
        state.set_selectors(vec![Selector::new(
            "settings",
            "Agent settings",
            "Choose a reported setting",
            vec![SelectorRow::new(
                "listed",
                "Listed setting",
                "current value",
            )],
        )]);
        assert!(state.open_selector("settings"));
        let _ = state.step(press(KeyCode::Char('l')));
        state.open_inspector(Inspector::new(
            "Operation failed",
            "the setting request failed",
        ));
        assert!(matches!(state.surface, Surface::Inspector(_)));
        assert!(state.step(press(KeyCode::Esc)).is_empty());
        assert!(matches!(
            &state.surface,
            Surface::Selector(list) if list.query == "l" && list.selected == Some(0)
        ));
        assert_eq!(state.editor().text(), draft);
        assert_eq!(state.editor().cursor_byte(), cursor);

        assert!(state.step(press(KeyCode::Esc)).is_empty());
        let _ = state.step(ctrl('p'));
        let _ = state.step(press(KeyCode::Char('s')));
        state.open_inspector(Inspector::new(
            "Operation failed",
            "the report request failed",
        ));
        assert!(state.step(ctrl('c')).is_empty());
        assert!(matches!(
            &state.surface,
            Surface::Palette(list) if list.query == "s"
        ));
    }

    #[test]
    fn failed_selector_mutation_restores_its_exact_source_and_success_clears_it() {
        let mut state = active_state();
        for index in 0..4 {
            state.apply(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(format!(
                    "retained row {index}"
                ))))
                .message_id(MessageId::new(format!("selector-anchor-{index}"))),
            ));
        }
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        cache.refresh(&state, 12, 8, &registry);
        state.sync_reading_layout(cache.layout_revision, &cache.positions, 6);
        let _ = state.step(press(KeyCode::PageUp));
        let anchor = state.reading_anchor.clone();
        assert!(anchor.is_some());

        let _ = state.step(paste("draft survives a failed selector mutation"));
        let _ = state.step(press(KeyCode::Left));
        let draft = state.editor().text().to_string();
        let cursor = state.editor().cursor_byte();
        state.set_commands(vec![Command::new(
            "Route chooser",
            "Open a temporary route operation",
            CommandOwner::BitRouter,
            CommandTarget::Report {
                id: "status".to_string(),
            },
        )]);
        state.set_selectors(vec![Selector::new(
            "config:model",
            "Agent setting · Model",
            "Select a reported model",
            vec![
                SelectorRow::new("target-alpha", "Target alpha", "first reported model"),
                SelectorRow::new("target-beta", "Target beta", "second reported model"),
            ],
        )]);

        let _ = state.step(ctrl('p'));
        let _ = state.step(press(KeyCode::Char('r')));
        assert!(state.open_selector("config:model"));
        let _ = state.step(press(KeyCode::Char('t')));
        let _ = state.step(press(KeyCode::Down));
        let effect = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &effect[..],
            [CodeEffect::Select { selector, id, custom: false }]
                if selector == "config:model" && id == "target-beta"
        ));
        assert!(matches!(
            &state.surface,
            Surface::Palette(list) if list.query == "r"
        ));

        assert!(!state.selector_mutation_failed("route"));
        assert!(state.selector_mutation_failed("config:model"));
        state.open_inspector(Inspector::new(
            "Operation failed",
            "agent rejected the model",
        ));
        assert!(state.step(press(KeyCode::Esc)).is_empty());
        assert!(matches!(
            &state.surface,
            Surface::Selector(list)
                if list.query == "t" && list.selected == Some(1)
        ));
        assert_eq!(state.editor().text(), draft);
        assert_eq!(state.editor().cursor_byte(), cursor);
        assert_eq!(state.reading_anchor, anchor);

        assert!(state.step(press(KeyCode::Esc)).is_empty());
        assert!(matches!(
            &state.surface,
            Surface::Palette(list) if list.query == "r"
        ));

        assert!(state.open_selector("config:model"));
        let _ = state.step(press(KeyCode::Char('t')));
        let _ = state.step(press(KeyCode::Down));
        let _ = state.step(press(KeyCode::Enter));
        state.selector_mutation_succeeded("config:model");
        assert!(state.pending_selector_mutation.is_none());
        assert!(matches!(
            &state.surface,
            Surface::Palette(list) if list.query == "r"
        ));
        state.open_inspector(Inspector::new("Unrelated report", "not a selector failure"));
        assert!(state.step(press(KeyCode::Esc)).is_empty());
        assert!(matches!(
            &state.surface,
            Surface::Palette(list) if list.query == "r"
        ));
    }

    #[test]
    fn completed_tool_cards_are_compact_but_the_inspector_keeps_full_diffs() {
        let mut state = active_state();
        let long_output = (1..=12)
            .map(|line| format!("command output line {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let diff = Diff::new("src/lib.rs", "new exact diff\nwith another line")
            .old_text("old exact diff\nwith prior line".to_string());
        state.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("tool-diff"), "Edit retained diff")
                .status(ToolCallStatus::Completed)
                .content(vec![
                    ToolCallContent::from(ContentBlock::Text(TextContent::new(long_output))),
                    ToolCallContent::from(diff),
                ]),
        ));
        let call = state.journal().entries().find_map(|item| match item.entry {
            Entry::Tool(call) => Some(call),
            Entry::Message(_) | Entry::Plan(_) => None,
        });
        let Some(call) = call else {
            assert!(
                state
                    .journal()
                    .entries()
                    .any(|item| matches!(item.entry, Entry::Tool(_)))
            );
            return;
        };
        let compact = compact_tool_lines(call, 80, 24, &Registry::default());
        let compact_text = compact.iter().map(line_text).collect::<Vec<_>>().join("\n");
        assert!(compact_text.contains("F4 inspects full tool output"));
        assert!(!compact_text.contains("command output line 12"));

        let _ = state.step(press(KeyCode::F(4)));
        let inspector = match &state.surface {
            Surface::Inspector(inspector) => inspector,
            _ => {
                assert!(matches!(state.surface, Surface::Inspector(_)));
                return;
            }
        };
        assert!(inspector.inspector.content.contains("old exact diff"));
        assert!(inspector.inspector.content.contains("new exact diff"));
        assert!(
            inspector
                .inspector
                .content
                .contains("command output line 12")
        );
    }

    #[test]
    fn operations_bootstrap_has_no_local_conversation_chrome() -> io::Result<()> {
        let mut state = CodeState::new(CodeStatus {
            title: "Remote operations · read-only · qa target".to_string(),
            ..CodeStatus::default()
        });
        state.set_operations_only(true);

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let screen = grid(terminal.backend());

        assert!(screen.contains("Remote operations"));
        assert!(screen.contains("Read-only operations"));
        assert!(screen.contains("Loading target status"));
        for local_marker in ["agent:", "route:", "activity:", "session cost:", "› "] {
            assert!(
                !screen.contains(local_marker),
                "unexpected local marker: {local_marker}"
            );
        }

        let _ = state.step(ctrl('p'));
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let palette = grid(terminal.backend());
        assert!(palette.contains("Commands"));
        assert!(!palette.contains("session cost:"));
        Ok(())
    }

    #[test]
    fn queue_editor_keeps_late_choices_visible_and_edits_exact_bytes() -> io::Result<()> {
        let mut state = active_state();
        start_working(&mut state, "working");
        for index in 0..12 {
            let prompt = format!("queued-{index:02} 界界 with a long first line\nsecond line");
            let _ = state.step(paste(&prompt));
            let _ = state.step(press(KeyCode::Tab));
        }
        state.surface = Surface::Queue { selected: 0 };
        for _ in 0..11 {
            let _ = state.step(press(KeyCode::Down));
        }
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 16))?;
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let rendered = grid(terminal.backend());
        assert!(rendered.contains("› queued-11"), "{rendered}");
        assert!(!rendered.contains("queued-00"), "{rendered}");
        assert!(rendered.contains("Enter/e edit"), "{rendered}");
        let _ = state.step(press(KeyCode::Enter));
        assert_eq!(
            state.editor.text(),
            "queued-11 界界 with a long first line\nsecond line"
        );
        assert_eq!(state.queue.len(), 11);
        Ok(())
    }

    #[test]
    fn status_and_composer_are_readable_at_supported_sizes() -> io::Result<()> {
        let mut state = CodeState::new(CodeStatus {
            title: "project".to_string(),
            agent: "very-long-agent-name-with-unicode-界".to_string(),
            route: "very-long-route-name-with-unicode-界".to_string(),
            activity: "working on a long tool invocation".to_string(),
        });
        let mut usage = UsageUpdate::new(10, 100);
        usage.cost = Some(WireCost::new(0.42, "USD"));
        let mut meta = serde_json::Map::new();
        meta.insert(
            cost::COST_PROVENANCE_META_KEY.to_string(),
            serde_json::Value::String(cost::COST_PROVENANCE_ROUTER.to_string()),
        );
        usage.meta = Some(meta);
        state.apply(SessionUpdate::UsageUpdate(usage));
        let _ = state.step(paste("ab界cd\nsecond composer line"));

        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let wide = grid(terminal.backend());
        assert!(wide.contains("agent:"));
        assert!(wide.contains("route:"));
        assert!(wide.contains("activity:"));
        assert!(wide.contains("session cost:"));
        assert!(wide.contains("(router)"));

        terminal.backend_mut().resize(40, 16);
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let narrow = grid(terminal.backend());
        assert!(narrow.contains("agent:"));
        assert!(narrow.contains("route:"));
        assert!(narrow.contains("activity:"));
        assert!(narrow.contains("session cost:"));

        let mut large_usage = UsageUpdate::new(20, 100);
        large_usage.cost = Some(WireCost::new(
            123_456_789.123_4,
            "VERY-LONG-CURRENCY-CODE-THAT-DOES-NOT-FIT",
        ));
        let mut large_meta = serde_json::Map::new();
        large_meta.insert(
            cost::COST_PROVENANCE_META_KEY.to_string(),
            serde_json::Value::String(cost::COST_PROVENANCE_ROUTER.to_string()),
        );
        large_usage.meta = Some(large_meta);
        state.apply(SessionUpdate::UsageUpdate(large_usage));
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let large_narrow = grid(terminal.backend());
        assert!(large_narrow.contains("session cost: router"));

        terminal.backend_mut().resize(80, 24);
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let large_wide = grid(terminal.backend());
        assert!(large_wide.contains("session cost: router"));
        assert!(large_wide.contains("activity:"));

        if std::env::var_os("BITROUTER_TUI_RENDER_GRID").is_some() {
            eprintln!("80x24\n{wide}\n\n40x16\n{narrow}\n\nlarge cost\n{large_wide}");
        }

        let layout = composer_layout("ab界cd", "ab界".len(), 3);
        assert!(layout.rows.len() >= 2);
        assert!(layout.cursor_row < layout.rows.len());
        assert!(layout.cursor_column >= 2);
        Ok(())
    }

    #[test]
    fn permission_labels_remain_visible_at_minimum_supported_size() -> io::Result<()> {
        let mut state = active_state();
        assert!(
            state
                .receive_permission(question("permission-1"))
                .is_empty()
        );
        let _ = state.step(press(KeyCode::F(2)));
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 16))?;
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let rendered = grid(terminal.backend());
        assert!(rendered.contains("Permission"));
        assert!(rendered.contains("[1] allow"));
        assert!(rendered.contains("[2] reject-always"));
        assert!(rendered.contains("[3] reject-once"));
        assert!(rendered.contains("Press a number"));
        if std::env::var_os("BITROUTER_TUI_RENDER_GRID").is_some() {
            eprintln!("40x16 permission\n{rendered}");
        }
        Ok(())
    }

    #[test]
    fn selected_long_permission_option_stays_visible_at_minimum_size() -> io::Result<()> {
        let mut state = active_state();
        let options = (1..=5)
            .map(|index| {
                PermissionOption::new(
                    PermissionOptionId::new(format!("option-{index}")),
                    format!(
                        "option {index} carries a deliberately long offered label for a narrow terminal"
                    ),
                    PermissionOptionKind::AllowOnce,
                )
            })
            .collect();
        let prompt = Prompt::new(
            "long-options",
            Some("Write a file with a long permission title".to_string()),
            "tool-long-options",
            None,
            options,
        );
        let context = ToolCallUpdate::new(
            ToolCallId::new("long-option-context"),
            ToolCallUpdateFields::default(),
        );
        assert!(
            state
                .receive_permission_with_context(prompt, context)
                .is_empty()
        );
        let _ = state.step(press(KeyCode::F(2)));
        let _ = state.step(press(KeyCode::Char('5')));

        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 16))?;
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let rendered = grid(terminal.backend());
        assert!(rendered.contains("› [5] option 5"));
        assert!(rendered.contains("Enter confirms selection"));
        assert!(rendered.contains("F4 inspects complete"));

        let effects = state.step(press(KeyCode::Enter));
        assert!(matches!(
            &effects[..],
            [CodeEffect::ResolvePermission {
                id,
                outcome: RequestPermissionOutcome::Selected(selection),
            }] if id == "long-options" && selection.option_id.0.as_ref() == "option-5"
        ));
        Ok(())
    }

    #[test]
    fn permission_presentation_sanitizes_agent_supplied_text() -> io::Result<()> {
        let mut state = active_state();
        let prompt = Prompt::new(
            "request-\u{1b}[31m",
            Some("Allow \u{1b}[32mwrite\nspoofed heading".to_string()),
            "tool-escape",
            None,
            vec![PermissionOption::new(
                PermissionOptionId::new("allow"),
                "Allow \u{1b}[33mthis change\nspoofed option",
                PermissionOptionKind::AllowOnce,
            )],
        );
        let context = ToolCallUpdate::new(
            ToolCallId::new("context-\u{1b}[34m"),
            ToolCallUpdateFields::default(),
        );
        assert!(
            state
                .receive_permission_with_context(prompt, context)
                .is_empty()
        );
        let _ = state.step(press(KeyCode::F(2)));

        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 16))?;
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let rendered = grid(terminal.backend());
        assert!(!rendered.contains('\u{1b}'));
        assert!(rendered.contains("␛[32mwrite"));
        assert!(rendered.contains("␛[33mthis change"));
        assert!(rendered.contains("context-␛[34m"));
        assert!(!rendered.contains("spoofed"));
        Ok(())
    }

    #[test]
    fn palette_keeps_a_selected_wrapped_choice_in_view() -> io::Result<()> {
        let mut state = CodeState::default();
        state.set_commands(
            (1..=12)
                .map(|index| {
                    Command::new(
                        format!("command-{index:02} has a label that wraps at narrow width"),
                        "A detailed row that also wraps in the result viewport",
                        CommandOwner::BitRouter,
                        CommandTarget::Report {
                            id: format!("report-{index}"),
                        },
                    )
                })
                .collect(),
        );
        state.apply(SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(Vec::new()),
        ));
        let _ = state.step(ctrl('p'));
        let _ = state.step(press(KeyCode::End));
        let Surface::Palette(list) = &state.surface else {
            return Err(io::Error::other("Ctrl-P did not open the command palette"));
        };
        assert_eq!(list.selected, Some(11));

        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(40, 16))?;
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let rendered = grid(terminal.backend());
        assert!(rendered.contains("› command-12"));

        let _ = state.step(press(KeyCode::Down));
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let after_down = grid(terminal.backend());
        assert!(after_down.contains("› command-12"));
        Ok(())
    }

    #[test]
    fn inspector_search_handles_unicode_lowercase_expansion() {
        assert_eq!(first_match_line("İé\nsecond", "é"), Some(0));
        assert_eq!(first_match_line("İ\nbefore\nÉclair", "écl"), Some(2));
    }

    #[test]
    fn long_streaming_history_draws_a_bounded_tail_and_keeps_the_reading_anchor() -> io::Result<()>
    {
        let mut state = active_state();
        for index in 0..40 {
            state.apply(SessionUpdate::AgentMessageChunk(
                ContentChunk::new(ContentBlock::Text(TextContent::new(format!(
                    "stream {index:02} retained transcript row"
                ))))
                .message_id(MessageId::new(format!("message-{index}"))),
            ));
        }
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let first_revision = cache.layout_revision;
        let tail = grid(terminal.backend());
        assert!(tail.contains("stream 39 retained transcript row"));
        assert!(!tail.contains("stream 00 retained transcript row"));

        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert_eq!(cache.layout_revision, first_revision);
        let _ = state.step(press(KeyCode::PageUp));
        let anchor = state.reading_anchor.clone();
        assert!(anchor.is_some());
        terminal.backend_mut().resize(40, 16);
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert_eq!(state.reading_anchor, anchor);
        assert!(grid(terminal.backend()).contains("Reading history"));
        state.apply(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(
                "stream 40 live update",
            )))
            .message_id(MessageId::new("message-40")),
        ));
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert_eq!(state.reading_anchor, anchor);
        assert!(state.new_activity);
        let reading = grid(terminal.backend());
        assert!(reading.contains("Reading history · new activity"));
        if std::env::var_os("BITROUTER_TUI_RENDER_GRID").is_some() {
            eprintln!("80x24 long history\n{reading}");
        }
        Ok(())
    }

    #[test]
    fn reading_location_survives_an_expanded_tool_before_the_anchor() -> io::Result<()> {
        let mut state = active_state();
        state.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("expanding-tool"), "Initial short output")
                .status(ToolCallStatus::Completed)
                .content(vec![ToolCallContent::from(ContentBlock::Text(
                    TextContent::new("short output"),
                ))]),
        ));
        let message = (1..=36)
            .map(|line| format!("anchored message row {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        state.apply(SessionUpdate::AgentMessageChunk(
            ContentChunk::new(ContentBlock::Text(TextContent::new(message)))
                .message_id(MessageId::new("anchored-message")),
        ));
        let registry = Registry::default();
        let mut cache = DocumentCache::default();
        let mut terminal = Terminal::new(TestBackend::new(80, 24))?;
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        let _ = state.step(press(KeyCode::PageUp));
        let anchor = state.reading_anchor.clone();
        assert!(matches!(
            &anchor,
            Some(ReadingAnchor { entry: EntryId::Message(id), .. }) if id.0.as_ref() == "anchored-message"
        ));

        let long_output = (1..=24)
            .map(|line| format!("expanded output row {line}"))
            .collect::<Vec<_>>()
            .join("\n");
        let mut fields = ToolCallUpdateFields::default();
        fields.content = Some(vec![ToolCallContent::from(ContentBlock::Text(
            TextContent::new(long_output),
        ))]);
        state.apply(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new("expanding-tool"),
            fields,
        )));
        terminal.backend_mut().resize(40, 16);
        terminal.draw(|frame| render_frame(frame, &mut state, &registry, &mut cache))?;
        assert_eq!(state.reading_anchor, anchor);
        let start = cache.start_for(&state, 4);
        let anchored_entry = cache.entries.iter().find(|entry| {
            let end = entry.start.saturating_add(entry.rows.len());
            entry.start <= start && start < end
        });
        assert!(matches!(
            anchored_entry,
            Some(entry) if matches!(&anchor, Some(anchor) if entry.id == anchor.entry)
        ));
        assert!(grid(terminal.backend()).contains("Reading history"));
        Ok(())
    }
}
