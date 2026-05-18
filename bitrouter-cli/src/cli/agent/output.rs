//! Event-to-stdout rendering for the headless agent driver.

use std::io::{self, Write};

use bitrouter_core::agents::event::AgentEvent;

use crate::cli::OutputFormat;

/// Render an `AgentEvent` to the given writer.
///
/// In text mode, message and thought chunks are streamed inline as
/// human-readable output; tool calls and permission requests render as
/// bracketed status lines; `TurnDone` writes a trailing newline.
///
/// In JSON mode, every event is emitted as a single newline-delimited
/// JSON object using the `Serialize` derive on `AgentEvent`.
pub fn write_event(w: &mut impl Write, event: &AgentEvent, format: OutputFormat) -> io::Result<()> {
    match format {
        OutputFormat::Json => {
            serde_json::to_writer(&mut *w, event).map_err(io::Error::other)?;
            writeln!(w)?;
            w.flush()
        }
        OutputFormat::Text => write_event_text(w, event),
    }
}

fn write_event_text(w: &mut impl Write, event: &AgentEvent) -> io::Result<()> {
    match event {
        AgentEvent::MessageChunk { text } => {
            write!(w, "{text}")?;
            w.flush()
        }
        AgentEvent::ThoughtChunk { text } => {
            // Thoughts to stderr so stdout stays clean for piping.
            eprint!("\x1b[2m{text}\x1b[0m");
            io::stderr().flush()
        }
        AgentEvent::ToolCall { title, status, .. } => {
            eprintln!("[tool] {title} ({status:?})");
            Ok(())
        }
        AgentEvent::ToolCallUpdate { title, status, .. } => {
            if let (Some(t), Some(s)) = (title.as_ref(), status.as_ref()) {
                eprintln!("[tool] {t} ({s:?})");
            } else if let Some(s) = status {
                eprintln!("[tool] ({s:?})");
            }
            Ok(())
        }
        AgentEvent::NonTextContent { description } => {
            eprintln!("[content] {description}");
            Ok(())
        }
        AgentEvent::PermissionRequest { request, .. } => {
            eprintln!("[permission] {}", request.title);
            Ok(())
        }
        AgentEvent::TurnDone { .. } => writeln!(w),
        AgentEvent::Error { message } => {
            eprintln!("error: {message}");
            Ok(())
        }
        AgentEvent::Disconnected | AgentEvent::HistoryReplayDone => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_core::agents::event::{StopReason, ToolCallStatus};

    #[test]
    fn json_emits_one_line_per_event() {
        let mut buf: Vec<u8> = Vec::new();
        write_event(
            &mut buf,
            &AgentEvent::MessageChunk {
                text: "hello".to_owned(),
            },
            OutputFormat::Json,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"type\":\"message_chunk\""));
        assert!(s.contains("\"text\":\"hello\""));
        assert!(s.ends_with('\n'));
    }

    #[test]
    fn text_writes_message_chunk_inline() {
        let mut buf: Vec<u8> = Vec::new();
        write_event(
            &mut buf,
            &AgentEvent::MessageChunk {
                text: "ok".to_owned(),
            },
            OutputFormat::Text,
        )
        .unwrap();
        assert_eq!(buf, b"ok");
    }

    #[test]
    fn text_turn_done_writes_newline() {
        let mut buf: Vec<u8> = Vec::new();
        write_event(
            &mut buf,
            &AgentEvent::TurnDone {
                stop_reason: StopReason::EndTurn,
            },
            OutputFormat::Text,
        )
        .unwrap();
        assert_eq!(buf, b"\n");
    }

    #[test]
    fn json_serializes_tool_call_status() {
        let mut buf: Vec<u8> = Vec::new();
        write_event(
            &mut buf,
            &AgentEvent::ToolCall {
                tool_call_id: "tc1".to_owned(),
                title: "read file".to_owned(),
                status: ToolCallStatus::InProgress,
            },
            OutputFormat::Json,
        )
        .unwrap();
        let s = String::from_utf8(buf).unwrap();
        assert!(s.contains("\"status\":\"in_progress\""));
    }
}
