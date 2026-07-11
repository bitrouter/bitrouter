//! Canonical request-timing values shared by the pipeline and plugins.

use std::time::{Duration, Instant};

use serde::{Deserialize, Serialize};

use crate::language_model::types::StreamPart;

/// Convert a duration to integer milliseconds without losing a positive
/// sub-millisecond measurement to truncation.
pub(crate) fn duration_millis(duration: Duration) -> u64 {
    if duration.is_zero() {
        return 0;
    }
    let millis = duration.as_millis().max(1);
    u64::try_from(millis).unwrap_or(u64::MAX)
}

pub(crate) fn elapsed_millis(started_at: Instant) -> u64 {
    duration_millis(started_at.elapsed())
}

/// The first semantic output produced by a streamed generation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum FirstTokenKind {
    /// A reasoning/thinking delta.
    Reasoning,
    /// A visible text delta.
    Text,
    /// A tool-call delta.
    Tool,
}

impl FirstTokenKind {
    /// Stable snake-case storage representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Reasoning => "reasoning",
            Self::Text => "text",
            Self::Tool => "tool",
        }
    }

    pub(crate) fn from_part(part: &StreamPart) -> Option<Self> {
        match part {
            StreamPart::ReasoningDelta { .. } => Some(Self::Reasoning),
            StreamPart::TextDelta { .. } => Some(Self::Text),
            StreamPart::ToolCallDelta { .. } => Some(Self::Tool),
            _ => None,
        }
    }
}

/// Time from the successful provider attempt start to its first semantic
/// streamed output.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct FirstTokenTiming {
    /// Elapsed provider time in milliseconds.
    pub latency_ms: u64,
    /// Which semantic delta arrived first.
    pub kind: FirstTokenKind,
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use super::duration_millis;

    #[test]
    fn positive_sub_millisecond_duration_rounds_to_one() {
        assert_eq!(duration_millis(Duration::from_nanos(1)), 1);
    }

    #[test]
    fn zero_duration_stays_zero() {
        assert_eq!(duration_millis(Duration::ZERO), 0);
    }

    #[test]
    fn whole_milliseconds_are_preserved() {
        assert_eq!(duration_millis(Duration::from_millis(42)), 42);
    }
}
