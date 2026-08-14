//! Turning ACP `session/update` notifications into terminal lines.
//!
//! This is the crate's whole rendering model, and it is deliberately a pure
//! function of the protocol: [`Transcript::apply`] takes one `SessionUpdate`
//! and returns the lines it produced. No I/O, no clock, no terminal — which
//! is why every variant below is testable without a terminal at all.
//!
//! # Streaming
//!
//! Message and thought chunks arrive in pieces that must read as one
//! paragraph, not one line per token. The transcript buffers them and emits a
//! line only when the agent moves on to something else, so wrapping happens
//! against the finished text rather than against an arbitrary chunk boundary.

use agent_client_protocol_schema::v1::{
    ContentBlock, SessionUpdate, ToolCallContent, ToolCallStatus,
};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};

/// Prefix marking the agent's reasoning, distinct from its answer.
const THOUGHT_PREFIX: &str = "· ";

/// What a tool call is doing, as one glyph.
///
/// A pending call and a failed one must never look alike: the whole reason to
/// render status is so a stalled turn is distinguishable from a broken one.
fn status_glyph(status: ToolCallStatus) -> (&'static str, Color) {
    match status {
        ToolCallStatus::Pending => ("◌", Color::DarkGray),
        ToolCallStatus::InProgress => ("◍", Color::Yellow),
        ToolCallStatus::Completed => ("●", Color::Green),
        ToolCallStatus::Failed => ("✗", Color::Red),
        // An unknown future status is reported as unknown, not quietly shown
        // as one of the four we do understand.
        _ => ("?", Color::Magenta),
    }
}

/// Accumulates streamed chunks and renders each update into finished lines.
#[derive(Debug, Default)]
pub struct Transcript {
    /// The in-flight agent message, if one is streaming.
    message: String,
    /// The in-flight agent reasoning, if one is streaming.
    thought: String,
}

impl Transcript {
    /// Render one update, returning the lines it completed.
    ///
    /// An empty result is normal and means "nothing is finished yet" — a
    /// chunk mid-sentence, or a variant this renderer does not draw.
    pub fn apply(&mut self, update: SessionUpdate) -> Vec<Line<'static>> {
        match update {
            SessionUpdate::AgentMessageChunk(chunk) => {
                // Symmetric with the thought arm below: each voice ends the
                // other. Without this a message resumed after a thought would
                // buffer behind it, and `flush` — which emits message before
                // thought — would then print them in the wrong order.
                let lines = self.flush_thought();
                self.message.push_str(&block_text(&chunk.content));
                lines
            }
            SessionUpdate::AgentThoughtChunk(chunk) => {
                // A thought interrupts a message: they are different voices
                // and must not be concatenated into one paragraph.
                let lines = self.flush_message();
                self.thought.push_str(&block_text(&chunk.content));
                lines
            }
            SessionUpdate::ToolCall(call) => {
                let mut lines = self.flush();
                let (glyph, color) = status_glyph(call.status);
                lines.push(Line::from(vec![
                    Span::styled(format!("{glyph} "), Style::default().fg(color)),
                    Span::raw(call.title),
                ]));
                lines.extend(diff_lines(&call.content));
                lines
            }
            SessionUpdate::ToolCallUpdate(update) => {
                let mut lines = self.flush();
                // Only what changed is present, so a bare id update renders
                // nothing rather than a card full of blanks.
                if let Some(status) = update.fields.status {
                    let (glyph, color) = status_glyph(status);
                    let title = update
                        .fields
                        .title
                        .unwrap_or_else(|| update.tool_call_id.0.to_string());
                    lines.push(Line::from(vec![
                        Span::styled(format!("{glyph} "), Style::default().fg(color)),
                        Span::raw(title),
                    ]));
                } else if let Some(title) = update.fields.title {
                    lines.push(Line::from(vec![Span::raw("  "), Span::raw(title)]));
                }
                if let Some(content) = &update.fields.content {
                    lines.extend(diff_lines(content));
                }
                lines
            }
            // Every other variant belongs to a pane this module does not own
            // (usage, plans, config). Flushing keeps ordering honest: whatever
            // was streaming finished before that update arrived.
            _ => self.flush(),
        }
    }

    /// Emit anything still buffered.
    ///
    /// **Must be called at the end of every turn**, not only when the session
    /// ends. Chunks accumulate until something ends them, and in ACP v1 the
    /// turn's end arrives on the `session/prompt` response — not as an update
    /// — so this buffer has no way to learn about it on its own. An agent
    /// whose turn is a plain answer sends message chunks and nothing else;
    /// leave those buffered and the reply is never drawn at all.
    ///
    /// Because each voice now flushes the other, at most one of the two is
    /// ever non-empty here, so the message-before-thought order below cannot
    /// reorder a real transcript.
    pub fn flush(&mut self) -> Vec<Line<'static>> {
        let mut lines = self.flush_message();
        lines.extend(self.flush_thought());
        lines
    }

    fn flush_message(&mut self) -> Vec<Line<'static>> {
        if self.message.trim().is_empty() {
            self.message.clear();
            return Vec::new();
        }
        let text = std::mem::take(&mut self.message);
        text.lines().map(|l| Line::from(l.to_string())).collect()
    }

    fn flush_thought(&mut self) -> Vec<Line<'static>> {
        if self.thought.trim().is_empty() {
            self.thought.clear();
            return Vec::new();
        }
        let text = std::mem::take(&mut self.thought);
        text.lines()
            .map(|l| {
                Line::from(Span::styled(
                    format!("{THOUGHT_PREFIX}{l}"),
                    Style::default()
                        .fg(Color::DarkGray)
                        .add_modifier(Modifier::ITALIC),
                ))
            })
            .collect()
    }
}

/// The text of a content block. Non-text blocks render empty rather than as a
/// placeholder — an image has no honest one-line spelling.
fn block_text(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        _ => String::new(),
    }
}

/// Render every diff in a tool call's content as `+`/`-` lines.
fn diff_lines(content: &[ToolCallContent]) -> Vec<Line<'static>> {
    let mut lines = Vec::new();
    for item in content {
        let ToolCallContent::Diff(diff) = item else {
            continue;
        };
        lines.push(Line::from(Span::styled(
            format!("  {}", diff.path.display()),
            Style::default().add_modifier(Modifier::BOLD),
        )));
        if let Some(old) = &diff.old_text {
            for removed in old.lines() {
                lines.push(Line::from(Span::styled(
                    format!("  -{removed}"),
                    Style::default().fg(Color::Red),
                )));
            }
        }
        for added in diff.new_text.lines() {
            lines.push(Line::from(Span::styled(
                format!("  +{added}"),
                Style::default().fg(Color::Green),
            )));
        }
    }
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{
        ContentChunk, Diff, TextContent, ToolCall, ToolCallId, ToolCallUpdate, ToolCallUpdateFields,
    };

    fn chunk(text: &str) -> ContentChunk {
        ContentChunk::new(ContentBlock::Text(TextContent::new(text.to_string())))
    }

    fn rendered(lines: &[Line<'static>]) -> String {
        lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Streamed chunks must read as one paragraph. Emitting a line per chunk
    /// would wrap at token boundaries, which is unreadable.
    #[test]
    fn message_chunks_join_into_one_line() {
        let mut transcript = Transcript::default();
        assert!(
            transcript
                .apply(SessionUpdate::AgentMessageChunk(chunk("Hello, ")))
                .is_empty(),
            "a mid-sentence chunk completes nothing"
        );
        transcript.apply(SessionUpdate::AgentMessageChunk(chunk("world.")));
        assert_eq!(rendered(&transcript.flush()), "Hello, world.");
    }

    /// Reasoning and answer are different voices and must not be concatenated.
    #[test]
    fn thought_chunks_render_distinctly_from_messages() {
        let mut transcript = Transcript::default();
        transcript.apply(SessionUpdate::AgentMessageChunk(chunk("the answer")));
        // The thought arrives after, so the message must flush first.
        let flushed = transcript.apply(SessionUpdate::AgentThoughtChunk(chunk("weighing it")));
        assert_eq!(rendered(&flushed), "the answer");

        let out = rendered(&transcript.flush());
        assert_eq!(out, format!("{THOUGHT_PREFIX}weighing it"));
        assert!(
            out.starts_with(THOUGHT_PREFIX),
            "reasoning must be visibly marked: {out:?}"
        );
    }

    /// A tool call renders its title and its status, and a diff renders as
    /// `-`/`+` lines rather than as a blob of JSON.
    #[test]
    fn a_tool_call_renders_title_status_and_diff() {
        let mut transcript = Transcript::default();
        let call = ToolCall::new(ToolCallId::new("t1"), "Edit src/lib.rs")
            .status(ToolCallStatus::InProgress)
            .content(vec![ToolCallContent::Diff(
                Diff::new("src/lib.rs", "let b = 2;").old_text("let a = 1;".to_string()),
            )]);
        let out = rendered(&transcript.apply(SessionUpdate::ToolCall(call)));

        assert!(out.contains("Edit src/lib.rs"), "{out:?}");
        assert!(
            out.contains("src/lib.rs"),
            "the diff names its file: {out:?}"
        );
        assert!(out.contains("-let a = 1;"), "the removed line: {out:?}");
        assert!(out.contains("+let b = 2;"), "the added line: {out:?}");
        // In progress, not finished — the two must not look alike.
        let (running, _) = status_glyph(ToolCallStatus::InProgress);
        assert!(out.contains(running), "{out:?}");
    }

    /// A status change renders; an update carrying only an id renders
    /// nothing, rather than a card full of blanks.
    #[test]
    fn a_tool_call_update_renders_only_what_changed() {
        let mut transcript = Transcript::default();
        let mut fields = ToolCallUpdateFields::default();
        fields.status = Some(ToolCallStatus::Failed);
        fields.title = Some("Edit src/lib.rs".to_string());
        let finished = ToolCallUpdate::new(ToolCallId::new("t1"), fields);
        let out = rendered(&transcript.apply(SessionUpdate::ToolCallUpdate(finished)));
        let (failed, _) = status_glyph(ToolCallStatus::Failed);
        assert!(out.contains(failed), "a failure must be visible: {out:?}");
        assert!(out.contains("Edit src/lib.rs"), "{out:?}");

        let bare = ToolCallUpdate::new(ToolCallId::new("t2"), ToolCallUpdateFields::default());
        assert!(
            transcript
                .apply(SessionUpdate::ToolCallUpdate(bare))
                .is_empty(),
            "an update with no changed field renders nothing"
        );
    }

    /// Interleaved voices must survive in the order they arrived.
    ///
    /// Found by driving a real session in tmux, not by these tests: the two
    /// arms were asymmetric — a thought ended a message but a message did not
    /// end a thought — so a message resumed after a thought queued behind it
    /// and `flush`, which emits message before thought, printed them
    /// backwards. Worse, with nothing after it to force a flush, the tail was
    /// dropped outright.
    #[test]
    fn interleaved_voices_keep_their_order_and_nothing_is_lost() {
        let mut transcript = Transcript::default();
        let mut rendered: Vec<String> = Vec::new();
        let mut collect = |lines: Vec<Line<'static>>| {
            for line in lines {
                rendered.push(
                    line.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>(),
                );
            }
        };

        collect(transcript.apply(SessionUpdate::AgentMessageChunk(chunk("first"))));
        collect(transcript.apply(SessionUpdate::AgentThoughtChunk(chunk("middle"))));
        collect(transcript.apply(SessionUpdate::AgentMessageChunk(chunk("last"))));
        // The turn ends here — exactly what the driver must do.
        collect(transcript.flush());

        let joined: Vec<&str> = rendered.iter().map(String::as_str).collect();
        assert_eq!(
            joined,
            vec!["first", "· middle", "last"],
            "every voice, in arrival order"
        );
    }

    /// The commonest turn there is: the agent answers and stops. Nothing else
    /// arrives to force a flush, so if the driver does not flush at the end of
    /// the turn the reply is never drawn — which is precisely what shipped.
    #[test]
    fn a_plain_answer_is_drawn_when_the_turn_ends() {
        let mut transcript = Transcript::default();
        assert!(
            transcript
                .apply(SessionUpdate::AgentMessageChunk(chunk("The answer is 42.")))
                .is_empty(),
            "a chunk alone completes no line — the turn's end is what does"
        );
        let lines = transcript.flush();
        let text: String = lines
            .first()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .unwrap_or_default();
        assert_eq!(text, "The answer is 42.");
    }

    /// Every status maps to its own glyph. A pending call that looked like a
    /// failed one would make a stalled turn indistinguishable from a broken
    /// one, which is the only reason to draw status at all.
    #[test]
    fn every_tool_status_is_distinguishable() {
        let glyphs: Vec<&str> = [
            ToolCallStatus::Pending,
            ToolCallStatus::InProgress,
            ToolCallStatus::Completed,
            ToolCallStatus::Failed,
        ]
        .into_iter()
        .map(|s| status_glyph(s).0)
        .collect();
        let unique: std::collections::BTreeSet<&str> = glyphs.iter().copied().collect();
        assert_eq!(unique.len(), glyphs.len(), "{glyphs:?}");
    }

    /// A non-text block contributes nothing rather than a placeholder.
    #[test]
    fn a_blank_message_produces_no_line() {
        let mut transcript = Transcript::default();
        transcript.apply(SessionUpdate::AgentMessageChunk(chunk("   ")));
        assert!(transcript.flush().is_empty());
    }
}
