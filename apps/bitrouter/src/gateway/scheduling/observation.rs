//! Bounded, success-conditioned measurements. No prompt or credential retention.

use std::io::Write;

use bitrouter_ai::types::{Prompt, StreamPart};
use tokio::time::{Duration, Instant};

use crate::gateway::error::{BitrouterError, Result};

pub(super) const SAMPLE_BYTES: u64 = 64;
pub(super) const SAMPLE_TTL: Duration = Duration::from_secs(300);

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct Shape {
    input: u8,
    output: u8,
    stream: bool,
}

impl Shape {
    pub(crate) fn from_prompt(prompt: &Prompt) -> Result<(Self, u64)> {
        struct Size(u64);
        impl Write for Size {
            fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
                self.0 = self
                    .0
                    .checked_add(bytes.len() as u64)
                    .ok_or_else(|| std::io::Error::other("request size overflow"))?;
                Ok(bytes.len())
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let mut size = Size(0);
        serde_json::to_writer(&mut size, prompt)
            .map_err(|_| BitrouterError::bad_request("cannot measure scheduling input"))?;
        let output = u64::from(prompt.params.max_tokens.unwrap_or(4096).min(32768));
        let work = size.0.div_ceil(4).saturating_add(output).clamp(256, 131072);
        Ok((
            Self {
                input: match size.0 {
                    0..=4096 => 0,
                    4097..=32768 => 1,
                    32769..=262144 => 2,
                    _ => 3,
                },
                output: match output {
                    0..=1024 => 0,
                    1025..=8192 => 1,
                    _ => 2,
                },
                stream: prompt.stream,
            },
            work,
        ))
    }
    pub(super) fn streaming(self) -> bool {
        self.stream
    }
}

pub(super) struct Sample {
    pub(super) shape: Shape,
    pub(super) at: Instant,
    pub(super) millis: u64,
}

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub(crate) struct Counters {
    pub(crate) response_headers: u64,
    pub(crate) completed: u64,
    pub(crate) rejected: u64,
    pub(crate) failed_terminal: u64,
    pub(crate) unsent: u64,
    pub(crate) unknown: u64,
    pub(crate) timeouts: u64,
    pub(crate) transport_failures: u64,
    pub(crate) invalid_responses: u64,
    pub(crate) dropped: u64,
    pub(crate) expired: u64,
    pub(crate) usage_reports: u64,
}

pub(super) fn useful(part: &StreamPart) -> bool {
    match part {
        StreamPart::TextDelta { text } | StreamPart::ReasoningDelta { text, .. } => {
            !text.is_empty()
        }
        StreamPart::ToolCallDelta {
            name, arguments, ..
        } => name.as_ref().is_some_and(|name| !name.is_empty()) || !arguments.is_empty(),
        _ => false,
    }
}
