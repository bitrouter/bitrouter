/// The action to take when a guardrail pattern matches.
///
/// Actions are ordered from loosest to strictest:
/// - **Warn** — log the match but allow content through unchanged.
/// - **Redact** — replace matched content with a placeholder.
/// - **Block** — reject the entire request/response with an error.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Action {
    /// Log a warning but allow the content through unchanged.
    Warn,
    /// Replace matched substrings with a placeholder (`[REDACTED]`).
    Redact,
    /// Reject the entire request or response with an error message.
    Block,
}

/// A single guardrail violation detected during inspection.
#[derive(Debug, Clone)]
pub struct Violation {
    /// The pattern that triggered this violation.
    pub pattern_id: crate::pattern::PatternId,
    /// Human-readable description of what was detected.
    pub description: &'static str,
    /// The action that was applied to this violation.
    pub action: Action,
    /// The matched substring (only populated for `Warn` and `Block`; empty for
    /// `Redact` since the content has already been replaced).
    pub matched: String,
}

/// The result of a guardrail inspection on content.
#[derive(Debug, Clone)]
pub struct InspectionResult {
    /// All violations detected during inspection.
    pub violations: Vec<Violation>,
    /// Whether the content was blocked (any pattern triggered `Block`).
    pub blocked: bool,
    /// The (possibly redacted) content after inspection.
    pub content: String,
}

impl InspectionResult {
    /// Returns `true` when no violations were found.
    pub fn is_clean(&self) -> bool {
        self.violations.is_empty()
    }
}

/// The placeholder used when redacting matched content.
pub const REDACTED_PLACEHOLDER: &str = "[REDACTED]";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inspection_result_is_clean_when_empty() {
        let result = InspectionResult {
            violations: vec![],
            blocked: false,
            content: "hello world".to_owned(),
        };
        assert!(result.is_clean());
    }

    #[test]
    fn inspection_result_is_not_clean_with_violations() {
        let result = InspectionResult {
            violations: vec![Violation {
                pattern_id: crate::pattern::PatternId::ApiKeys,
                description: "API keys from common providers",
                action: Action::Warn,
                matched: "sk-abc123".to_owned(),
            }],
            blocked: false,
            content: "hello sk-abc123".to_owned(),
        };
        assert!(!result.is_clean());
    }
}
