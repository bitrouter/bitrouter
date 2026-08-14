//! The session as a retained document, patched in place.
//!
//! # Why retained, and not append-only
//!
//! [`crate::transcript::Transcript`] turns each update into finished lines and
//! forgets it. That is append-only against a protocol whose entities are
//! **patchable**: one tool call going pending → in-progress → completed emits
//! three lines, because by the time the second arrives the first belongs to the
//! terminal.
//!
//! So this keeps the entities instead of their rendering. [`Journal::apply`]
//! returns `()` — nothing for a caller to order, nothing to commit early — and
//! whoever draws reads the whole document. A status change is a field
//! assignment on a tool call that is already on screen.
//!
//! Two properties follow, and both are load-bearing:
//!
//! - **First-seen order is stable.** A patch to an early entity keeps its
//!   place; only genuinely new entities extend the document.
//! - **There is nothing to flush.** `Transcript` has to be flushed at every
//!   turn end or a plain answer is never drawn, because a turn ends on the
//!   `session/prompt` *response*, which never appears on the update stream. An
//!   in-flight message here is simply a [`Message`] with `complete: false`,
//!   already in the document and already on screen.
//!
//! # The sticky keying rule
//!
//! `ContentChunk::message_id` is optional in ACP v1, so chunks cannot simply be
//! grouped by id. The rule is:
//!
//! - each voice has at most one **open run**;
//! - a chunk carrying `Some(id)` that differs from the open run's id closes it
//!   and opens a new one;
//! - a chunk carrying `None` **continues** the open run rather than starting a
//!   synthesized one;
//! - a run also closes when the other voice speaks, or when a tool call lands
//!   — anything that takes a place in document order.
//!
//! Under ACP v2 the id is required and the fallback dies with it.

use std::collections::HashMap;

use agent_client_protocol_schema::v1::{
    AvailableCommand, ContentBlock, ContentChunk, MessageId, Plan, SessionConfigOption,
    SessionModeId, SessionUpdate, ToolCall, ToolCallId, ToolCallUpdate, UsageUpdate,
};

use crate::permission::Prompt;

/// Ids synthesized for chunks that carry none are prefixed with this. An agent
/// id colliding with it would have to contain a colon-delimited `journal`
/// namespace, which no agent has reason to mint.
const SYNTHESIZED: &str = "journal:run:";

/// Who is speaking. A message and a thought are different voices and must
/// never be concatenated into one paragraph.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Voice {
    /// The user's own message, echoed back by the agent.
    User,
    /// The agent's answer.
    Agent,
    /// The agent's reasoning.
    Thought,
}

/// One run of chunks under a single id.
#[derive(Debug, Clone)]
pub struct Message {
    /// Which voice this run belongs to.
    pub voice: Voice,
    /// Everything received on this run so far.
    pub text: String,
    /// False while more chunks may still arrive. An incomplete message is
    /// drawn like any other; this is what a renderer uses to mark it as still
    /// arriving, not a reason to withhold it.
    pub complete: bool,
}

/// What a document position points at.
///
/// This crate's own enum rather than a protocol type: the protocol has no
/// notion of "the things that occupy a place in the transcript".
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum EntryId {
    /// A run of message or thought chunks.
    Message(MessageId),
    /// A tool call.
    Tool(ToolCallId),
}

/// A document entry, borrowed for rendering.
#[derive(Debug, Clone, Copy)]
pub enum Entry<'a> {
    /// A run of message or thought chunks.
    Message(&'a Message),
    /// A tool call, as the protocol has it after every patch so far.
    Tool(&'a ToolCall),
}

/// The session, retained.
#[derive(Debug, Default)]
pub struct Journal {
    /// Document order, first-seen. Patches never reorder.
    order: Vec<EntryId>,
    messages: HashMap<MessageId, Message>,
    tools: HashMap<ToolCallId, ToolCall>,
    plan: Option<Plan>,
    commands: Vec<AvailableCommand>,
    mode: Option<SessionModeId>,
    config: Vec<SessionConfigOption>,
    title: Option<String>,
    usage: Option<UsageUpdate>,
    /// Not a `SessionUpdate`: permission arrives on its own request channel.
    pending_permission: Option<Prompt>,
    /// The open run, if any — at most one across all voices, because the
    /// other voice speaking is what closes it.
    open: Option<(Voice, MessageId)>,
    /// How many ids have been synthesized for chunks that carried none.
    synthesized: usize,
}

impl Journal {
    /// Patch the journal from a session update.
    ///
    /// Returns nothing on purpose: the writer reads the whole document, so
    /// there is no per-update line list for a caller to mis-order, and no
    /// buffered text for it to forget to flush.
    pub fn apply(&mut self, update: SessionUpdate) {
        match update {
            SessionUpdate::UserMessageChunk(chunk) => self.chunk(Voice::User, chunk),
            SessionUpdate::AgentMessageChunk(chunk) => self.chunk(Voice::Agent, chunk),
            SessionUpdate::AgentThoughtChunk(chunk) => self.chunk(Voice::Thought, chunk),
            SessionUpdate::ToolCall(call) => {
                // A tool call takes a place in document order, so whatever was
                // streaming ended before it.
                self.close_run();
                self.insert_tool(call);
            }
            SessionUpdate::ToolCallUpdate(update) => {
                self.close_run();
                self.patch_tool(update);
            }
            // The rest are not document entries: they are the footer and the
            // session's own state, replaced wholesale by whatever arrived last.
            // None of them can reorder the transcript, so none of them closes a
            // run.
            SessionUpdate::Plan(plan) => self.plan = Some(plan),
            SessionUpdate::AvailableCommandsUpdate(update) => {
                self.commands = update.available_commands;
            }
            SessionUpdate::CurrentModeUpdate(update) => self.mode = Some(update.current_mode_id),
            SessionUpdate::ConfigOptionUpdate(update) => self.config = update.config_options,
            SessionUpdate::SessionInfoUpdate(update) => match update.title {
                // `Undefined` means the field was absent — this update is
                // about something else and says nothing about the title.
                // `Null` means the agent cleared it, which is a change.
                agent_client_protocol_schema::MaybeUndefined::Undefined => {}
                agent_client_protocol_schema::MaybeUndefined::Null => self.title = None,
                agent_client_protocol_schema::MaybeUndefined::Value(title) => {
                    self.title = Some(title);
                }
            },
            SessionUpdate::UsageUpdate(usage) => self.usage = Some(usage),
            // A variant this build does not know about. Ignored rather than
            // guessed at: `SessionUpdate` is `#[non_exhaustive]`, and the
            // unstable plan operations are behind a feature the workspace does
            // not enable.
            _ => {}
        }
    }

    /// Set or clear the open permission prompt.
    ///
    /// Separate from [`Journal::apply`] because `session/request_permission`
    /// is a request, not an update: it arrives on its own channel and is
    /// resolved by an answer rather than superseded by the next notification.
    pub fn set_pending_permission(&mut self, prompt: Option<Prompt>) {
        self.pending_permission = prompt;
    }

    /// The document, in first-seen order.
    pub fn entries(&self) -> impl Iterator<Item = Entry<'_>> {
        self.order.iter().filter_map(|id| match id {
            EntryId::Message(id) => self.messages.get(id).map(Entry::Message),
            EntryId::Tool(id) => self.tools.get(id).map(Entry::Tool),
        })
    }

    /// The agent's plan, if it sent one.
    pub fn plan(&self) -> Option<&Plan> {
        self.plan.as_ref()
    }

    /// The slash commands the agent offers.
    pub fn commands(&self) -> &[AvailableCommand] {
        &self.commands
    }

    /// The session's current mode, if the agent reports one.
    pub fn mode(&self) -> Option<&SessionModeId> {
        self.mode.as_ref()
    }

    /// The session's configuration options, as last advertised.
    pub fn config(&self) -> &[SessionConfigOption] {
        &self.config
    }

    /// The session title, if one has been set.
    pub fn title(&self) -> Option<&str> {
        self.title.as_deref()
    }

    /// Context-window and cost, as last reported.
    pub fn usage(&self) -> Option<&UsageUpdate> {
        self.usage.as_ref()
    }

    /// The permission question waiting for an answer, if any.
    pub fn pending_permission(&self) -> Option<&Prompt> {
        self.pending_permission.as_ref()
    }

    /// Append a chunk to the open run, or start a new one.
    fn chunk(&mut self, voice: Voice, chunk: ContentChunk) {
        let text = block_text(&chunk.content);
        let incoming = chunk.message_id;
        let open = self.open.clone();
        let continues = match (&open, &incoming) {
            (Some((open_voice, open_id)), Some(id)) => *open_voice == voice && open_id == id,
            (Some((open_voice, _)), None) => *open_voice == voice,
            (None, _) => false,
        };

        let id = match (continues, open) {
            (true, Some((_, open_id))) => open_id,
            _ => {
                self.close_run();
                let id = match incoming {
                    Some(id) => id,
                    None => self.synthesize(),
                };
                // An id seen before is a message being *reopened*, not a
                // second entry: it keeps the place it already has.
                if !self.messages.contains_key(&id) {
                    self.messages.insert(
                        id.clone(),
                        Message {
                            voice,
                            text: String::new(),
                            complete: false,
                        },
                    );
                    self.order.push(EntryId::Message(id.clone()));
                }
                self.open = Some((voice, id.clone()));
                id
            }
        };

        if let Some(message) = self.messages.get_mut(&id) {
            message.complete = false;
            message.text.push_str(&text);
        }
    }

    /// Close the open run, if there is one.
    fn close_run(&mut self) {
        if let Some((_, id)) = self.open.take()
            && let Some(message) = self.messages.get_mut(&id)
        {
            message.complete = true;
        }
    }

    /// An id for a run whose chunks carried none.
    fn synthesize(&mut self) -> MessageId {
        self.synthesized = self.synthesized.saturating_add(1);
        MessageId::from(format!("{SYNTHESIZED}{}", self.synthesized))
    }

    fn insert_tool(&mut self, call: ToolCall) {
        let id = call.tool_call_id.clone();
        if !self.tools.contains_key(&id) {
            self.order.push(EntryId::Tool(id.clone()));
        }
        self.tools.insert(id, call);
    }

    /// Apply the fields an update actually carried, and only those.
    ///
    /// An update for a call that was never announced still creates one: the
    /// alternative is dropping content the agent sent, and an untitled call is
    /// more honest than a missing one.
    fn patch_tool(&mut self, update: ToolCallUpdate) {
        let id = update.tool_call_id;
        if !self.tools.contains_key(&id) {
            self.order.push(EntryId::Tool(id.clone()));
            self.tools
                .insert(id.clone(), ToolCall::new(id.clone(), String::new()));
        }
        let Some(call) = self.tools.get_mut(&id) else {
            return;
        };
        let fields = update.fields;
        if let Some(kind) = fields.kind {
            call.kind = kind;
        }
        if let Some(status) = fields.status {
            call.status = status;
        }
        if let Some(title) = fields.title {
            call.title = title;
        }
        if let Some(content) = fields.content {
            call.content = content;
        }
        if let Some(locations) = fields.locations {
            call.locations = locations;
        }
        if fields.raw_input.is_some() {
            call.raw_input = fields.raw_input;
        }
        if fields.raw_output.is_some() {
            call.raw_output = fields.raw_output;
        }
    }
}

/// The text of a content block. Non-text blocks contribute nothing here;
/// describing them is the renderer's job, not the journal's.
fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        _ => String::new(),
    }
}

#[cfg(test)]
mod tests {
    use agent_client_protocol_schema::v1::{
        AvailableCommandsUpdate, ConfigOptionUpdate, CurrentModeUpdate, Diff, PermissionOption,
        PermissionOptionId, PermissionOptionKind, PlanEntry, PlanEntryPriority, PlanEntryStatus,
        SessionConfigBoolean, SessionConfigId, SessionConfigKind, SessionInfoUpdate, TextContent,
        ToolCallContent, ToolCallStatus, ToolCallUpdateFields, ToolKind,
    };

    use super::*;

    fn chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())))
    }

    fn keyed(text: &str, id: &str) -> ContentChunk {
        let mut chunk = chunk(text);
        chunk.message_id = Some(MessageId::from(id.to_string()));
        chunk
    }

    /// The one tool call in the document, if there is one.
    fn tool(journal: &Journal) -> Option<&ToolCall> {
        journal.entries().find_map(|entry| match entry {
            Entry::Tool(call) => Some(call),
            Entry::Message(_) => None,
        })
    }

    /// Every message in the document, in order.
    fn messages(journal: &Journal) -> Vec<&Message> {
        journal
            .entries()
            .filter_map(|entry| match entry {
                Entry::Message(message) => Some(message),
                Entry::Tool(_) => None,
            })
            .collect()
    }

    /// The document as `(voice-or-tool, text)` pairs, which is what every
    /// ordering assertion below is really about.
    fn document(journal: &Journal) -> Vec<(String, String)> {
        journal
            .entries()
            .map(|entry| match entry {
                Entry::Message(message) => (
                    format!("{:?}", message.voice).to_lowercase(),
                    message.text.clone(),
                ),
                Entry::Tool(call) => ("tool".to_string(), call.title.clone()),
            })
            .collect()
    }

    // ── the eight `transcript.rs` tests, as the journal has them ────────────

    /// `message_chunks_join_into_one_line`: chunks are one message, not one
    /// entry per token.
    #[test]
    fn message_chunks_join_into_one_message() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("Hello, ")));
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("world.")));
        assert_eq!(
            document(&journal),
            vec![("agent".to_string(), "Hello, world.".to_string())]
        );
    }

    /// `thought_chunks_render_distinctly_from_messages`: the two voices are
    /// separate entries, so no renderer can concatenate them by accident.
    #[test]
    fn a_thought_is_a_separate_entry_from_a_message() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("the answer")));
        journal.apply(SessionUpdate::AgentThoughtChunk(chunk("weighing it")));
        assert_eq!(
            document(&journal),
            vec![
                ("agent".to_string(), "the answer".to_string()),
                ("thought".to_string(), "weighing it".to_string()),
            ]
        );
    }

    /// `a_tool_call_renders_title_status_and_diff`: the journal keeps the call
    /// itself, so everything a renderer needs is still there afterwards.
    #[test]
    fn a_tool_call_is_retained_whole() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("t1"), "Edit src/lib.rs")
                .kind(ToolKind::Edit)
                .status(ToolCallStatus::InProgress)
                .content(vec![ToolCallContent::Diff(
                    Diff::new("src/lib.rs", "let b = 2;").old_text("let a = 1;".to_string()),
                )]),
        ));

        let call = tool(&journal);
        assert_eq!(
            call.map(|call| call.title.as_str()),
            Some("Edit src/lib.rs")
        );
        assert_eq!(call.map(|call| call.kind), Some(ToolKind::Edit));
        assert_eq!(
            call.map(|call| call.status),
            Some(ToolCallStatus::InProgress)
        );
        assert_eq!(
            call.map(|call| call.content.len()),
            Some(1),
            "the diff is kept, not rendered away"
        );
    }

    /// `a_tool_call_update_renders_only_what_changed` — and the defect the
    /// journal exists to fix. Three status changes are **one** entry, patched,
    /// not three lines in scrollback.
    #[test]
    fn a_tool_call_update_patches_in_place() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("t1"), "Edit src/lib.rs").status(ToolCallStatus::Pending),
        ));
        for status in [ToolCallStatus::InProgress, ToolCallStatus::Completed] {
            let mut fields = ToolCallUpdateFields::default();
            fields.status = Some(status);
            journal.apply(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new("t1"),
                fields,
            )));
        }

        assert_eq!(
            journal.entries().count(),
            1,
            "one call, however often it changed"
        );
        let call = tool(&journal);
        assert_eq!(
            call.map(|call| call.status),
            Some(ToolCallStatus::Completed)
        );
        assert_eq!(
            call.map(|call| call.title.as_str()),
            Some("Edit src/lib.rs"),
            "a field the update did not carry is not cleared"
        );
    }

    /// The other half of the same test: an update carrying nothing changes
    /// nothing, rather than writing a card full of blanks.
    #[test]
    fn a_bare_update_changes_nothing_it_was_not_given() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::ToolCall(
            ToolCall::new(ToolCallId::new("t1"), "Read src/lib.rs")
                .status(ToolCallStatus::Completed),
        ));
        journal.apply(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
            ToolCallId::new("t1"),
            ToolCallUpdateFields::default(),
        )));

        let call = tool(&journal);
        assert_eq!(
            call.map(|call| call.title.as_str()),
            Some("Read src/lib.rs")
        );
        assert_eq!(
            call.map(|call| call.status),
            Some(ToolCallStatus::Completed)
        );
    }

    /// `interleaved_voices_keep_their_order_and_nothing_is_lost`.
    #[test]
    fn interleaved_voices_keep_their_order_and_nothing_is_lost() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("first")));
        journal.apply(SessionUpdate::AgentThoughtChunk(chunk("middle")));
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("last")));

        assert_eq!(
            document(&journal),
            vec![
                ("agent".to_string(), "first".to_string()),
                ("thought".to_string(), "middle".to_string()),
                ("agent".to_string(), "last".to_string()),
            ],
            "a resumed voice is a new run, in arrival order"
        );
    }

    /// `a_plain_answer_is_drawn_when_the_turn_ends` — except that there is
    /// nothing to end. The reply is in the document from the first chunk,
    /// which is the whole point of dropping `flush`.
    #[test]
    fn a_plain_answer_needs_no_flush() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("The answer is 42.")));

        let text: Vec<&str> = messages(&journal)
            .iter()
            .map(|message| message.text.as_str())
            .collect();
        assert_eq!(text, vec!["The answer is 42."]);
        assert_eq!(
            messages(&journal).first().map(|message| message.complete),
            Some(false),
            "still streaming — and still on screen, which is the difference"
        );
    }

    /// `every_tool_status_is_distinguishable` moves to the renderer, since
    /// glyphs are a rendering concern. What the journal owes is that the
    /// status it was told is the status it keeps.
    #[test]
    fn every_status_survives_a_patch() {
        for status in [
            ToolCallStatus::Pending,
            ToolCallStatus::InProgress,
            ToolCallStatus::Completed,
            ToolCallStatus::Failed,
        ] {
            let mut journal = Journal::default();
            let mut fields = ToolCallUpdateFields::default();
            fields.status = Some(status);
            journal.apply(SessionUpdate::ToolCallUpdate(ToolCallUpdate::new(
                ToolCallId::new("t1"),
                fields,
            )));
            assert_eq!(tool(&journal).map(|call| call.status), Some(status));
        }
    }

    /// `a_blank_message_produces_no_line`: a non-text block contributes no
    /// text. It still opens a run — the agent did speak — and the renderer
    /// decides what an empty message looks like.
    #[test]
    fn a_non_text_block_contributes_no_text() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("   ")));
        assert_eq!(
            document(&journal),
            vec![("agent".to_string(), "   ".to_string())]
        );
    }

    // ── the sticky keying rule ─────────────────────────────────────────────

    /// The rule in full, on one interleaved stream: `None` continues the open
    /// run, a differing `Some` closes it and opens another, and the same
    /// `Some` after that continues *its* run.
    ///
    /// Keying per chunk instead would produce five entries here; keying only
    /// on `Some` would lose the unkeyed chunks or synthesize a run for each.
    #[test]
    fn message_keying_is_sticky_across_none_and_some_chunks() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(keyed("a1 ", "m1")));
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("a2 ")));
        journal.apply(SessionUpdate::AgentMessageChunk(keyed("a3", "m1")));
        journal.apply(SessionUpdate::AgentMessageChunk(keyed("b1 ", "m2")));
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("b2")));

        assert_eq!(
            document(&journal),
            vec![
                ("agent".to_string(), "a1 a2 a3".to_string()),
                ("agent".to_string(), "b1 b2".to_string()),
            ]
        );
    }

    /// An unkeyed run belongs to the voice that opened it. The other voice
    /// speaking closes it, so an unkeyed thought never lands inside a message.
    #[test]
    fn an_unkeyed_chunk_does_not_cross_voices() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("answer ")));
        journal.apply(SessionUpdate::AgentThoughtChunk(chunk("reasoning")));
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("continued")));

        assert_eq!(
            document(&journal),
            vec![
                ("agent".to_string(), "answer ".to_string()),
                ("thought".to_string(), "reasoning".to_string()),
                ("agent".to_string(), "continued".to_string()),
            ]
        );
    }

    /// A tool call closes the open run too: it takes a place in document
    /// order, so text after it must not join text before it.
    #[test]
    fn a_tool_call_closes_the_open_run() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("before ")));
        journal.apply(SessionUpdate::ToolCall(ToolCall::new(
            ToolCallId::new("t1"),
            "Read src/lib.rs",
        )));
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("after")));

        assert_eq!(
            document(&journal),
            vec![
                ("agent".to_string(), "before ".to_string()),
                ("tool".to_string(), "Read src/lib.rs".to_string()),
                ("agent".to_string(), "after".to_string()),
            ]
        );
    }

    /// A closed run is marked complete; the open one is not.
    #[test]
    fn closing_a_run_marks_it_complete() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(chunk("done")));
        journal.apply(SessionUpdate::AgentThoughtChunk(chunk("still going")));

        let complete: Vec<bool> = messages(&journal)
            .iter()
            .map(|message| message.complete)
            .collect();
        assert_eq!(
            complete,
            vec![true, false],
            "the closed run is complete, the open one is not"
        );
    }

    /// A message id seen again keeps the place it already had. Patches never
    /// reorder — the property the whole document depends on.
    #[test]
    fn a_reopened_message_keeps_its_place() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::AgentMessageChunk(keyed("first ", "m1")));
        journal.apply(SessionUpdate::AgentThoughtChunk(chunk("interrupting")));
        journal.apply(SessionUpdate::AgentMessageChunk(keyed("resumed", "m1")));

        assert_eq!(
            document(&journal),
            vec![
                ("agent".to_string(), "first resumed".to_string()),
                ("thought".to_string(), "interrupting".to_string()),
            ],
            "the resumed message stays where it first appeared"
        );
    }

    // ── the fields that are not document entries ───────────────────────────

    /// Everything the footer and §9's surfaces read is kept, and the last
    /// update wins.
    #[test]
    fn the_session_s_own_state_is_kept_and_replaced_wholesale() {
        let mut journal = Journal::default();
        assert!(journal.plan().is_none());
        assert!(journal.commands().is_empty());
        assert!(journal.mode().is_none());
        assert!(journal.config().is_empty());
        assert!(journal.title().is_none());
        assert!(journal.usage().is_none());

        journal.apply(SessionUpdate::Plan(Plan::new(vec![PlanEntry::new(
            "write the wrap",
            PlanEntryPriority::High,
            PlanEntryStatus::Pending,
        )])));
        journal.apply(SessionUpdate::AvailableCommandsUpdate(
            AvailableCommandsUpdate::new(vec![AvailableCommand::new("route", "change provider")]),
        ));
        journal.apply(SessionUpdate::CurrentModeUpdate(CurrentModeUpdate::new(
            SessionModeId::new("plan"),
        )));
        journal.apply(SessionUpdate::ConfigOptionUpdate(ConfigOptionUpdate::new(
            vec![SessionConfigOption::new(
                SessionConfigId::new("thinking"),
                "Extended thinking",
                SessionConfigKind::Boolean(SessionConfigBoolean::new(true)),
            )],
        )));
        journal.apply(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title("the first title".to_string()),
        ));
        journal.apply(SessionUpdate::UsageUpdate(UsageUpdate::new(100, 200_000)));

        assert_eq!(journal.plan().map(|p| p.entries.len()), Some(1));
        assert_eq!(journal.commands().len(), 1);
        assert_eq!(journal.mode().map(ToString::to_string), Some("plan".into()));
        assert_eq!(journal.config().len(), 1);
        assert_eq!(journal.title(), Some("the first title"));
        assert_eq!(journal.usage().map(|u| u.used), Some(100));

        // Later updates replace, rather than accumulate.
        journal.apply(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title("renamed".to_string()),
        ));
        journal.apply(SessionUpdate::UsageUpdate(UsageUpdate::new(150, 200_000)));
        assert_eq!(journal.title(), Some("renamed"));
        assert_eq!(journal.usage().map(|u| u.used), Some(150));

        // And none of them took a place in the transcript.
        assert!(document(&journal).is_empty());
    }

    /// A title the agent did not mention is not a title it cleared. The
    /// distinction is `MaybeUndefined`'s whole reason to exist.
    #[test]
    fn an_absent_title_is_not_a_cleared_title() {
        let mut journal = Journal::default();
        journal.apply(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().title("kept".to_string()),
        ));
        // An update about something else entirely.
        journal.apply(SessionUpdate::SessionInfoUpdate(
            SessionInfoUpdate::new().updated_at("2026-08-14T00:00:00Z".to_string()),
        ));
        assert_eq!(journal.title(), Some("kept"));

        let mut cleared = SessionInfoUpdate::new();
        cleared.title = agent_client_protocol_schema::MaybeUndefined::Null;
        journal.apply(SessionUpdate::SessionInfoUpdate(cleared));
        assert_eq!(journal.title(), None, "an explicit null does clear it");
    }

    /// Permission is set and cleared by the caller, not by the stream.
    #[test]
    fn a_pending_permission_is_set_and_cleared_by_its_owner() {
        let mut journal = Journal::default();
        assert!(journal.pending_permission().is_none());

        journal.set_pending_permission(Some(Prompt::new(
            Some("Write src/lib.rs".to_string()),
            "t1",
            vec![PermissionOption::new(
                PermissionOptionId::new("allow"),
                "Allow",
                PermissionOptionKind::AllowOnce,
            )],
        )));
        assert!(journal.pending_permission().is_some());

        journal.set_pending_permission(None);
        assert!(
            journal.pending_permission().is_none(),
            "an answered question stops being asked"
        );
    }
}
