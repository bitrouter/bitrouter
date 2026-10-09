//! Bounded public context projection shared by every routing adapter.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Write};

use bitrouter_ai::types::{Content, Prompt, Role, ToolResultOutput};

/// A visible observation. These values are classifier input, never authority.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Signal {
    /// Semantic content kind, independent of transport or harness name.
    pub kind: String,
    /// Public text or serialized public tool value.
    pub text: String,
}

/// Optional explicit task facts admitted by the input adapter.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Task {
    /// Current task, independent of worker identity.
    pub objective: String,
    /// Conditions that define completion.
    pub acceptance_criteria: Vec<String>,
}

/// Source-independent semantic input. Rich HTTP history and a Core work unit
/// can supply the same facts. Context mutation rights are supplied separately.
#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct Input {
    /// Explicit task facts when available; omission is not a different mode.
    pub task: Option<Task>,
    /// Chronologically ordered visible observations.
    pub signals: Vec<Signal>,
    /// True only when the host has established completeness of the source.
    pub complete: bool,
    /// Whether the bounded semantic projection omitted public text.
    pub truncated: bool,
}

impl Input {
    /// Project public text and ordinary tool exchanges. Private reasoning,
    /// provider metadata, media payloads and opaque continuation handles never
    /// enter the semantic backend. Their absence cannot imply complete history.
    pub fn from_prompt(prompt: &Prompt, max_text_bytes: usize) -> Self {
        let mut input = Self::default();
        let mut remaining = max_text_bytes;
        // Reserve bounded space for current instructions; spend history space
        // newest first so an old tool dump cannot hide the current workflow.
        let instruction_budget = prompt
            .system
            .as_ref()
            .map_or(0, |text| text.len().min(max_text_bytes / 4));
        remaining = remaining.saturating_sub(instruction_budget);
        for message in prompt.messages.iter().rev() {
            let role = match message.role {
                Role::System => "instruction",
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
            };
            for part in message.content.iter().rev() {
                match part {
                    Content::Text { text, .. } => input.push(role, text, &mut remaining),
                    Content::ToolCall {
                        name,
                        arguments,
                        provider_executed: false,
                        dynamic: false,
                        ..
                    } => {
                        input.push("tool_arguments", arguments, &mut remaining);
                        input.push("tool_name", name, &mut remaining);
                    }
                    Content::ToolResult {
                        output,
                        dynamic: false,
                        ..
                    } => match output {
                        ToolResultOutput::Text { value } => {
                            input.push("tool_result", value, &mut remaining)
                        }
                        ToolResultOutput::ErrorText { value } => {
                            input.push("tool_error", value, &mut remaining)
                        }
                        ToolResultOutput::Json { value }
                        | ToolResultOutput::ErrorJson { value } => {
                            // The output is ordinary tool data, not provider metadata.
                            let kind = if matches!(output, ToolResultOutput::ErrorJson { .. }) {
                                "tool_error"
                            } else {
                                "tool_result"
                            };
                            let (text, truncated) = json_text(value, remaining);
                            input.truncated |= truncated;
                            input.push(kind, &text, &mut remaining);
                        }
                        ToolResultOutput::ExecutionDenied { reason } => input.push(
                            "tool_denied",
                            reason.as_deref().unwrap_or("execution denied"),
                            &mut remaining,
                        ),
                        ToolResultOutput::Content { .. } => input.truncated = true,
                    },
                    Content::File { .. } => input.truncated = true,
                    _ => {}
                }
            }
        }
        remaining = remaining.saturating_add(instruction_budget);
        if let Some(system) = &prompt.system {
            input.push("instruction", system, &mut remaining);
        }
        input.signals.reverse();
        input
    }

    fn push(&mut self, kind: &str, text: &str, remaining: &mut usize) {
        let mut end = text.len().min(*remaining);
        while !text.is_char_boundary(end) {
            end = end.saturating_sub(1);
        }
        self.truncated |= end < text.len();
        if end > 0 {
            self.signals.push(Signal {
                kind: kind.into(),
                text: text[..end].into(),
            });
        }
        *remaining = remaining.saturating_sub(end);
    }
}

fn json_text(value: &Value, limit: usize) -> (String, bool) {
    struct BoundedJson {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl Write for BoundedJson {
        fn write(&mut self, buffer: &[u8]) -> io::Result<usize> {
            let count = buffer
                .len()
                .min(self.limit.saturating_sub(self.bytes.len()));
            self.bytes.extend_from_slice(&buffer[..count]);
            if count < buffer.len() {
                return Err(io::Error::other("semantic input bound reached"));
            }
            Ok(count)
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let mut writer = BoundedJson {
        bytes: Vec::new(),
        limit,
    };
    let truncated = serde_json::to_writer(&mut writer, value).is_err();
    // Truncation can split a UTF-8 code point; retain only the valid prefix.
    let valid =
        std::str::from_utf8(&writer.bytes).map_or_else(|error| error.valid_up_to(), str::len);
    writer.bytes.truncate(valid);
    (
        String::from_utf8_lossy(&writer.bytes).into_owned(),
        truncated,
    )
}
