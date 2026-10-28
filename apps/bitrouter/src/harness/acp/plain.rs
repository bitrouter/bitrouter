//! Plain ACP transcripts for redirected output. No terminal renderer or viewport.
//! Updates patch the shared journal; callers write only after a turn settles.

use std::borrow::Cow;
use std::io::{self, Write};

use agent_client_protocol_schema::v1::{
    ContentBlock, Diff, EmbeddedResourceResource, Plan, PlanEntryPriority, PlanEntryStatus,
    SessionUpdate, ToolCall, ToolCallContent, ToolCallStatus,
};
use similar::{ChangeTag, TextDiff};

use super::journal::{Entry, Journal, Voice};

/// A retained transcript with an append-only output cursor, shared across turns.
#[derive(Default)]
pub struct Transcript {
    journal: Journal,
    written: usize,
}

impl Transcript {
    pub fn apply(&mut self, update: SessionUpdate) {
        self.journal.apply(update);
    }

    /// Only rows not already handed to the output sink. Protocol metadata is
    /// retained by the journal but never mistaken for assistant text.
    pub fn unwritten(&mut self) -> Vec<String> {
        let mut lines = Vec::new();
        for item in self.journal.entries() {
            match item.entry {
                Entry::Message(message) => match message.voice {
                    Voice::User => {
                        lines.push(String::new());
                        lines.extend(message.text.split('\n').map(|line| format!("> {line}")));
                        lines.push(String::new());
                    }
                    Voice::Agent => lines.extend(message.text.lines().map(str::to_owned)),
                    Voice::Thought => {
                        lines.extend(message.text.lines().map(|line| format!("· {line}")));
                    }
                },
                Entry::Tool(call) => lines.extend(tool(call)),
                Entry::Plan(value) => lines.extend(plan(value)),
            }
        }
        let fresh = lines.get(self.written..).unwrap_or_default().to_vec();
        self.written = lines.len();
        fresh
    }
}

pub fn write(out: &mut impl Write, lines: &[String]) -> io::Result<()> {
    for line in lines {
        writeln!(out, "{line}")?;
    }
    Ok(())
}

fn tool(call: &ToolCall) -> Vec<String> {
    let glyph = match call.status {
        ToolCallStatus::Pending => "◌",
        ToolCallStatus::InProgress => "◍",
        ToolCallStatus::Completed => "●",
        ToolCallStatus::Failed => "✗",
        _ => "?",
    };
    let command = command(call);
    let title = match command.as_deref() {
        Some(command)
            if call.title.is_empty()
                || call.title == command
                || shell_words::split(&call.title)
                    .ok()
                    .is_some_and(|words| words.len() == 1 && words[0] == command) =>
        {
            "Command"
        }
        _ if call.title.is_empty() => &call.tool_call_id.0,
        _ => &call.title,
    };
    let mut lines = vec![format!("{glyph} {title}")];
    if let Some(command) = command {
        lines.extend(command.lines().map(|line| format!("  {line}")));
    }
    for item in &call.content {
        match item {
            ToolCallContent::Content(value) => lines.extend(content(&value.content)),
            ToolCallContent::Diff(value) => lines.extend(diff(value)),
            ToolCallContent::Terminal(_) => {}
            _ => {}
        }
    }
    lines
}

fn command(call: &ToolCall) -> Option<Cow<'_, str>> {
    let input = call.raw_input.as_ref()?;
    let value = input.get("command").or_else(|| input.get("cmd"))?;
    if let Some(command) = value.as_str() {
        return Some(Cow::Borrowed(command));
    }
    let argv: Vec<&str> = value
        .as_array()?
        .iter()
        .map(serde_json::Value::as_str)
        .collect::<Option<_>>()?;
    if let [shell, flag, script] = argv.as_slice()
        && matches!(
            shell.rsplit('/').next(),
            Some("sh" | "bash" | "zsh" | "fish" | "dash")
        )
        && matches!(*flag, "-c" | "-lc" | "-ic")
    {
        return Some(Cow::Borrowed(script));
    }
    (!argv.is_empty()).then(|| Cow::Owned(shell_words::join(argv)))
}

fn plan(plan: &Plan) -> Vec<String> {
    let mut lines = vec![format!("plan · {} steps", plan.entries.len())];
    lines.extend(plan.entries.iter().map(|entry| {
        let glyph = match entry.status {
            PlanEntryStatus::Completed => "✓",
            PlanEntryStatus::InProgress => "◍",
            PlanEntryStatus::Pending => "·",
            _ => "?",
        };
        let priority = match entry.priority {
            PlanEntryPriority::High => " (high)",
            PlanEntryPriority::Low => " (low)",
            _ => "",
        };
        format!("  {glyph} {}{priority}", entry.content)
    }));
    lines
}

fn content(block: &ContentBlock) -> Vec<String> {
    const MAX_LINES: usize = 40;
    let ContentBlock::Text(text) = block else {
        return vec![format!("  {}", describe(block))];
    };
    let mut lines: Vec<_> = text
        .text
        .lines()
        .take(MAX_LINES)
        .map(|line| format!("  {line}"))
        .collect();
    let dropped = text.text.lines().count().saturating_sub(MAX_LINES);
    if dropped > 0 {
        lines.push(format!("  … {} more lines", thousands(dropped)));
    }
    lines
}

fn describe(block: &ContentBlock) -> String {
    match block {
        ContentBlock::Text(text) => text.text.clone(),
        ContentBlock::Image(value) => {
            format!("[image {} {}]", value.mime_type, bytes(value.data.len()))
        }
        ContentBlock::Audio(value) => {
            format!("[audio {} {}]", value.mime_type, bytes(value.data.len()))
        }
        ContentBlock::ResourceLink(value) => format!("[resource: {}]", value.uri),
        ContentBlock::Resource(value) => match &value.resource {
            EmbeddedResourceResource::TextResourceContents(value) => {
                format!("[resource: {}]", value.uri)
            }
            EmbeddedResourceResource::BlobResourceContents(value) => {
                format!("[resource: {} {}]", value.uri, bytes(value.blob.len()))
            }
            _ => "[resource]".into(),
        },
        _ => "[unrecognised content]".into(),
    }
}

fn bytes(encoded: usize) -> String {
    let decoded = encoded.saturating_mul(3) / 4;
    let unit = if decoded < 1024 * 1024 {
        1024
    } else {
        1024 * 1024
    };
    if decoded < 1024 {
        format!("{decoded} B")
    } else {
        format!(
            "{}.{} {}",
            decoded / unit,
            (decoded % unit) * 10 / unit,
            if unit == 1024 { "KB" } else { "MB" }
        )
    }
}

fn thousands(count: usize) -> String {
    let digits = count.to_string();
    let mut out = String::new();
    for (index, digit) in digits.chars().enumerate() {
        if index > 0 && (digits.len() - index).is_multiple_of(3) {
            out.push(',');
        }
        out.push(digit);
    }
    out
}

fn diff(value: &Diff) -> Vec<String> {
    let path = std::path::absolute(&value.path).unwrap_or_else(|_| value.path.clone());
    let mut lines = vec![format!("  {}", path.display())];
    let old = value.old_text.as_deref().unwrap_or_default();
    let text = TextDiff::from_lines(old, &value.new_text);
    let mut previous_end = 0;
    let mut body = Vec::new();
    for group in text.grouped_ops(3) {
        let Some(first) = group.first() else { continue };
        if first.old_range().start > previous_end {
            body.push(format!(
                "  … {} unchanged lines",
                thousands(first.old_range().start - previous_end)
            ));
        }
        for op in &group {
            for change in text.iter_changes(op) {
                let marker = match change.tag() {
                    ChangeTag::Delete => "-",
                    ChangeTag::Insert => "+",
                    ChangeTag::Equal => " ",
                };
                body.push(format!(
                    "  {marker}{}",
                    change.value().trim_end_matches(['\r', '\n'])
                ));
            }
            previous_end = op.old_range().end;
        }
    }
    if old.lines().count() > previous_end {
        body.push(format!(
            "  … {} unchanged lines",
            thousands(old.lines().count() - previous_end)
        ));
    }
    if body.len() > 22 {
        let dropped = body.len() - 22;
        body.truncate(22);
        body.push(format!("  … {} more lines", thousands(dropped)));
    }
    lines.extend(body);
    lines
}

#[cfg(test)]
mod tests {
    use super::*;
    use agent_client_protocol_schema::v1::{ContentChunk, MessageId, TextContent, ToolCallUpdate};

    fn chunk(text: &str, id: &str) -> SessionUpdate {
        let mut chunk = ContentChunk::new(ContentBlock::Text(TextContent::new(text)));
        chunk.message_id = Some(MessageId::from(id.to_string()));
        SessionUpdate::AgentMessageChunk(chunk)
    }

    #[test]
    fn a_transcript_hands_out_each_row_once() {
        let mut transcript = Transcript::default();
        transcript.apply(chunk("first", "m1"));
        assert_eq!(transcript.unwritten(), ["first"]);
        assert!(transcript.unwritten().is_empty());
        transcript.apply(chunk("second", "m2"));
        assert_eq!(transcript.unwritten(), ["second"]);
    }

    #[test]
    fn patches_settle_once_in_document_order_without_terminal_bytes()
    -> Result<(), serde_json::Error> {
        let mut transcript = Transcript::default();
        transcript.apply(chunk("reading ", "m1"));
        transcript.apply(chunk("now", "m1"));
        transcript.apply(SessionUpdate::ToolCall(ToolCall::new(
            "tool-1",
            "Read file",
        )));
        let update: ToolCallUpdate = serde_json::from_value(serde_json::json!({
            "toolCallId":"tool-1", "status":"completed", "content":[{"type":"content","content":{"type":"text","text":"result"}}]
        }))?;
        transcript.apply(SessionUpdate::ToolCallUpdate(update));
        transcript.apply(chunk("done", "m2"));
        let rows = transcript.unwritten();
        assert_eq!(rows, ["reading now", "● Read file", "  result", "done"]);
        assert!(transcript.unwritten().is_empty());
        let mut bytes = Vec::new();
        assert!(write(&mut bytes, &rows).is_ok());
        assert!(!bytes.contains(&0x1b));
        Ok(())
    }

    #[test]
    fn wire_transcript_keeps_plans_commands_diffs_and_resource_descriptions()
    -> Result<(), serde_json::Error> {
        let mut transcript = Transcript::default();
        for value in [
            serde_json::json!({"sessionUpdate":"agent_thought_chunk","content":{"type":"text","text":"Inspecting"}}),
            serde_json::json!({"sessionUpdate":"plan","entries":[{"content":"Inspect","priority":"high","status":"pending"}]}),
            serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"cmd","title":"Run checks","kind":"execute","rawInput":{"command":["sh","-lc","cargo check"]}}),
            serde_json::json!({"sessionUpdate":"tool_call_update","toolCallId":"cmd","status":"completed","content":[{"type":"content","content":{"type":"text","text":"checked"}},{"type":"content","content":{"type":"image","mimeType":"image/png","data":"AAAA"}}]}),
            serde_json::json!({"sessionUpdate":"tool_call","toolCallId":"edit","title":"Edit file","kind":"edit","status":"completed","content":[{"type":"diff","path":"plain-fixture.txt","oldText":"before\n","newText":"after\n"}]}),
            serde_json::json!({"sessionUpdate":"plan","entries":[{"content":"Inspect","priority":"high","status":"completed"}]}),
        ] {
            transcript.apply(serde_json::from_value(value)?);
        }
        let lines = transcript.unwritten();
        assert_eq!(
            &lines[..6],
            [
                "· Inspecting",
                "plan · 1 steps",
                "  ✓ Inspect (high)",
                "● Run checks",
                "  cargo check",
                "  checked"
            ]
        );
        assert!(lines.iter().any(|line| line == "  [image image/png 3 B]"));
        assert!(lines.iter().any(|line| line.ends_with("plain-fixture.txt")));
        assert!(lines.iter().any(|line| line == "  -before"));
        assert!(lines.iter().any(|line| line == "  +after"));
        assert!(transcript.unwritten().is_empty());
        Ok(())
    }
}
