//! Business contract for checks on a named router's entry request.
//!
//! Inputs contain bounded projected text and coverage evidence, without a
//! transport envelope. The host owns router binding, scheduling and receipts.

use std::sync::Arc;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::error::{BitrouterError, Result};

/// Maximum fragments in the projected entry request.
pub const MAX_CONTENT_FRAGMENTS: usize = 4096;
/// Largest configurable projected-text byte budget.
pub const MAX_INPUT_BYTES: u64 = 4 * 1024 * 1024;
/// Longest configurable invocation deadline, including queue admission.
pub const MAX_TIMEOUT_MS: u64 = 30_000;

/// Role attached to a canonical checker fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContentRole {
    /// Out-of-band system instructions.
    System,
    /// End-user content.
    User,
    /// Prior assistant content.
    Assistant,
    /// Tool results or approval responses.
    Tool,
}

/// The canonical kind of one checker input fragment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ContentFragmentKind {
    /// Plain message or system text.
    Text,
    /// Prior assistant reasoning text.
    Reasoning,
    /// Tool-call arguments.
    ToolCall,
    /// A tool result.
    ToolResult,
    /// A tool-approval decision.
    ToolApproval,
}

/// Coverage scope for the first request-check contract.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestCheckCoverageScope {
    /// Textual fragments on the entry request only. Media payloads, server-tool
    /// turns, and other nested requests are outside the scope.
    EntryRequestText,
}

/// Whether the checker input was complete within its declared scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum RequestCheckCoverageStatus {
    /// Every entry-request text fragment fit within the byte and count bounds.
    CompleteWithinScope,
    /// Entry-request text exceeded a byte or fragment bound; no checker was
    /// invoked.
    InputTooLarge,
}

/// Typed evidence for what an invocation covered.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestCheckCoverage {
    /// Fixed entry-request coverage scope.
    pub scope: RequestCheckCoverageScope,
    /// Total text bytes projected for the checker.
    pub text_bytes: u64,
    /// Number of projected text fragments.
    pub text_fragments: u64,
    /// Entry-request media fragments intentionally excluded from payloads.
    pub excluded_media_fragments: u64,
    /// Completeness within the fixed scope.
    pub status: RequestCheckCoverageStatus,
}

/// One typed fragment passed to a request-check extension.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ContentFragment {
    /// Conversation role of this fragment.
    pub role: ContentRole,
    /// Canonical content kind.
    pub kind: ContentFragmentKind,
    /// Textual content, when this kind has a text representation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<String>,
}

/// Bounded entry-request text passed to trusted extension code.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Input {
    /// Canonical textual fragments after the named router's defaults.
    pub content: Vec<ContentFragment>,
    /// Scope and completeness of the projected input.
    pub coverage: RequestCheckCoverage,
}

/// Business decision returned by an extension's request check.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    /// Permit the request to continue to model selection.
    Allow,
    /// Stop the request before provider dispatch.
    Deny {
        /// Bounded machine-readable code suitable for receipts.
        reason_code: String,
    },
}

impl Decision {
    /// Reject malformed denial codes before recording a decision.
    pub fn validate(&self) -> Result<()> {
        if let Self::Deny { reason_code } = self
            && (reason_code.is_empty()
                || reason_code.len() > 64
                || !reason_code.bytes().all(|byte| {
                    byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'-' | b':')
                }))
        {
            return Err(BitrouterError::bad_request(
                "request-check reason code must be 1–64 ASCII letters, digits, or . _ - :",
            ));
        }
        Ok(())
    }
}

/// Synchronous business callback. Hosts must bound concurrency and keep these
/// callbacks off async workers. A started callback cannot be forcibly cancelled;
/// an elapsed deadline only stops the host from waiting for its result.
pub type Callback = dyn Fn(&Input) -> Decision + Send + Sync + 'static;

/// One statically linked implementation consumed by a host at startup.
#[derive(Clone)]
pub struct Registration {
    /// Code/rules revision that must match the configured binding.
    pub revision: String,
    /// Trusted business implementation.
    pub callback: Arc<Callback>,
}

impl Registration {
    /// Construct an implementation. Hosts must still validate the revision and
    /// its configuration match before executing it.
    pub fn new(revision: impl Into<String>, callback: Arc<Callback>) -> Self {
        Self {
            revision: revision.into(),
            callback,
        }
    }
}

/// Validate a code/rules revision consistently in configuration and registration.
pub fn validate_revision(revision: &str) -> Result<()> {
    if revision.is_empty()
        || revision.len() > 128
        || !revision.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'.' | b'_' | b'+' | b'-' | b'/')
        })
    {
        return Err(BitrouterError::bad_request(
            "native revision must be 1–128 ASCII letters, digits, or . _ + - /",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denial_codes_are_bounded_receipt_metadata() {
        for (reason_code, valid) in [
            ("policy:block".to_owned(), true),
            ("a".repeat(64), true),
            ("a".repeat(65), false),
            (String::new(), false),
            ("contains secret text".to_owned(), false),
            ("规则".to_owned(), false),
        ] {
            assert_eq!(Decision::Deny { reason_code }.validate().is_ok(), valid);
        }
        assert!(Decision::Allow.validate().is_ok());
    }
}
