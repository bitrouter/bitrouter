//! Typed request-check contracts for named-router entry requests.
//!
//! Request checks cover only the canonical request that entered the pipeline.
//! Server-tool turns and other nested model calls are outside this contract.

use std::io::Write;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::extension::request_check::{
    ContentFragment, ContentFragmentKind, ContentRole, Decision, Input, RequestCheckCoverage,
    RequestCheckCoverageScope, RequestCheckCoverageStatus,
};
use crate::language_model::types::{
    Content, Prompt, Role, ToolResultContentPart, ToolResultOutput,
};

/// A checker binding frozen with a named router for one request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct RequestCheckBinding {
    /// Checker name resolved by the host runtime.
    pub checker_id: String,
    /// Redaction-safe digest of the effective checker binding.
    pub binding_digest: String,
    /// Maximum projected text bytes passed to the checker.
    pub max_input_bytes: u64,
    /// Per-invocation deadline enforced by the host runtime.
    pub timeout_ms: u64,
}

impl From<Role> for ContentRole {
    fn from(value: Role) -> Self {
        match value {
            Role::System => Self::System,
            Role::User => Self::User,
            Role::Assistant => Self::Assistant,
            Role::Tool => Self::Tool,
        }
    }
}

/// A business decision with the registered implementation revision supplied by the host.
/// The pipeline validates the decision before proceeding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CheckerResult {
    /// Decision returned by the extension callback.
    pub decision: Decision,
    /// Registered revision supplied by the host, never by the callback.
    pub revision: String,
}

/// Stable checker failure classification. Raw implementation errors are deliberately
/// excluded from this host boundary.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum CheckerFailureKind {
    /// The effective input exceeded the configured byte limit.
    InputTooLarge,
    /// The invocation deadline elapsed.
    Timeout,
    /// The checker could not be scheduled.
    Unavailable,
    /// The checker returned an invalid business decision.
    InvalidResponse,
    /// The host runtime could not resolve the configured checker.
    NotConfigured,
    /// An internal execution failure occurred.
    Internal,
}

/// A fail-closed checker invocation failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CheckerFailure {
    /// Stable failure classification.
    pub kind: CheckerFailureKind,
    /// Optional bounded diagnostic suitable for operators.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// Host runner for statically linked request-check extensions.
#[async_trait]
pub trait RequestCheckerRunner: Send + Sync {
    /// Evaluate one configured entry-request invocation. Request and router
    /// identities stay outside the runner input. The pipeline validates the
    /// returned business decision before proceeding.
    async fn check(
        &self,
        binding: RequestCheckBinding,
        input: Input,
    ) -> std::result::Result<CheckerResult, CheckerFailure>;
}

const MAX_PROJECTED_FRAGMENTS: u64 = crate::extension::request_check::MAX_CONTENT_FRAGMENTS as u64;

struct ContentProjection {
    fragments: Vec<ContentFragment>,
    text_bytes: u64,
    text_fragments: u64,
    excluded_media_fragments: u64,
    max_input_bytes: u64,
}

impl ContentProjection {
    fn new(excluded_media_fragments: u64, max_input_bytes: u64) -> Self {
        Self {
            fragments: Vec::new(),
            text_bytes: 0,
            text_fragments: 0,
            excluded_media_fragments,
            max_input_bytes,
        }
    }

    fn push(
        &mut self,
        role: ContentRole,
        kind: ContentFragmentKind,
        text: &str,
    ) -> Result<(), RequestCheckCoverage> {
        self.reserve_fragment()?;
        self.text_bytes = self.text_bytes.saturating_add(text.len() as u64);
        if self.text_bytes > self.max_input_bytes {
            return Err(self.input_too_large());
        }
        self.fragments.push(ContentFragment {
            role,
            kind,
            text: Some(text.to_owned()),
        });
        Ok(())
    }

    fn push_owned(
        &mut self,
        role: ContentRole,
        kind: ContentFragmentKind,
        text: String,
    ) -> Result<(), RequestCheckCoverage> {
        self.reserve_fragment()?;
        self.text_bytes = self.text_bytes.saturating_add(text.len() as u64);
        if self.text_bytes > self.max_input_bytes {
            return Err(self.input_too_large());
        }
        self.fragments.push(ContentFragment {
            role,
            kind,
            text: Some(text),
        });
        Ok(())
    }

    fn push_json(
        &mut self,
        role: ContentRole,
        kind: ContentFragmentKind,
        value: &serde_json::Value,
    ) -> Result<(), RequestCheckCoverage> {
        self.reserve_fragment()?;
        let remaining = self.max_input_bytes.saturating_sub(self.text_bytes);
        let Some(text) = serialize_json_bounded(value, remaining) else {
            self.text_bytes = self.max_input_bytes.saturating_add(1);
            return Err(self.input_too_large());
        };
        self.text_bytes = self.text_bytes.saturating_add(text.len() as u64);
        self.fragments.push(ContentFragment {
            role,
            kind,
            text: Some(text),
        });
        Ok(())
    }

    fn reserve_fragment(&mut self) -> Result<(), RequestCheckCoverage> {
        self.text_fragments = self.text_fragments.saturating_add(1);
        if self.text_fragments > MAX_PROJECTED_FRAGMENTS {
            return Err(self.input_too_large());
        }
        Ok(())
    }

    fn input_too_large(&self) -> RequestCheckCoverage {
        self.coverage(RequestCheckCoverageStatus::InputTooLarge)
    }

    fn coverage(&self, status: RequestCheckCoverageStatus) -> RequestCheckCoverage {
        RequestCheckCoverage {
            scope: RequestCheckCoverageScope::EntryRequestText,
            text_bytes: self.text_bytes,
            text_fragments: self.text_fragments,
            excluded_media_fragments: self.excluded_media_fragments,
            status,
        }
    }

    fn finish(self) -> (Vec<ContentFragment>, RequestCheckCoverage) {
        let coverage = self.coverage(RequestCheckCoverageStatus::CompleteWithinScope);
        (self.fragments, coverage)
    }
}

struct BoundedJsonWriter {
    bytes: Vec<u8>,
    remaining: usize,
}

impl Write for BoundedJsonWriter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if bytes.len() > self.remaining {
            return Err(std::io::Error::other(
                "request-check JSON exceeds its remaining byte limit",
            ));
        }
        self.bytes.extend_from_slice(bytes);
        self.remaining -= bytes.len();
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

fn serialize_json_bounded(value: &serde_json::Value, max_bytes: u64) -> Option<String> {
    let remaining = match usize::try_from(max_bytes) {
        Ok(limit) => limit,
        Err(_) => usize::MAX,
    };
    let mut writer = BoundedJsonWriter {
        bytes: Vec::new(),
        remaining,
    };
    serde_json::to_writer(&mut writer, value).ok()?;
    String::from_utf8(writer.bytes).ok()
}

/// Convert the effective canonical prompt into bounded, typed checker input.
/// Media payloads and URLs are excluded and counted in coverage evidence.
pub(crate) fn content_fragments(
    prompt: &Prompt,
    max_input_bytes: u64,
) -> Result<(Vec<ContentFragment>, RequestCheckCoverage), RequestCheckCoverage> {
    let excluded_media_fragments = count_excluded_media(prompt);
    let mut projection = ContentProjection::new(excluded_media_fragments, max_input_bytes);
    if let Some(system) = prompt.system.as_ref() {
        projection.push(ContentRole::System, ContentFragmentKind::Text, system)?;
    }
    for message in &prompt.messages {
        let role = ContentRole::from(message.role);
        for content in &message.content {
            match content {
                Content::Text { text, .. } => {
                    projection.push(role, ContentFragmentKind::Text, text)?
                }
                Content::Reasoning { text, .. } => {
                    projection.push(role, ContentFragmentKind::Reasoning, text)?
                }
                Content::File { .. } => {}
                Content::ToolCall { arguments, .. } => {
                    projection.push(role, ContentFragmentKind::ToolCall, arguments)?
                }
                Content::ToolResult { output, .. } => match output {
                    ToolResultOutput::Text { value } | ToolResultOutput::ErrorText { value } => {
                        projection.push(role, ContentFragmentKind::ToolResult, value)?;
                    }
                    ToolResultOutput::Json { value } | ToolResultOutput::ErrorJson { value } => {
                        projection.push_json(role, ContentFragmentKind::ToolResult, value)?;
                    }
                    ToolResultOutput::ExecutionDenied { reason } => {
                        projection.push(
                            role,
                            ContentFragmentKind::ToolResult,
                            reason.as_deref().unwrap_or("Tool call execution denied."),
                        )?;
                    }
                    ToolResultOutput::Content { value } => {
                        for part in value {
                            match part {
                                ToolResultContentPart::Text { text } => {
                                    projection.push(role, ContentFragmentKind::ToolResult, text)?
                                }
                                ToolResultContentPart::Media { .. }
                                | ToolResultContentPart::FileId { .. } => {}
                            }
                        }
                    }
                },
                Content::ToolApprovalRequest { .. } => {}
                Content::ToolApprovalResponse {
                    approved, reason, ..
                } => {
                    if let Some(reason) = reason {
                        projection.push(role, ContentFragmentKind::ToolApproval, reason)?;
                    } else {
                        projection.push_owned(
                            role,
                            ContentFragmentKind::ToolApproval,
                            approved.to_string(),
                        )?;
                    }
                }
                Content::Source { .. } => {}
            }
        }
    }
    Ok(projection.finish())
}

fn count_excluded_media(prompt: &Prompt) -> u64 {
    prompt.messages.iter().fold(0_u64, |total, message| {
        message.content.iter().fold(total, |count, content| {
            let excluded = match content {
                Content::File { .. } => 1,
                Content::ToolResult {
                    output: ToolResultOutput::Content { value },
                    ..
                } => value
                    .iter()
                    .filter(|part| {
                        matches!(
                            part,
                            ToolResultContentPart::Media { .. }
                                | ToolResultContentPart::FileId { .. }
                        )
                    })
                    .count() as u64,
                Content::Text { .. }
                | Content::Reasoning { .. }
                | Content::ToolCall { .. }
                | Content::ToolResult { .. }
                | Content::Source { .. }
                | Content::ToolApprovalRequest { .. }
                | Content::ToolApprovalResponse { .. } => 0,
            };
            count.saturating_add(excluded)
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::types::{
        DataContent, GenerationParams, Message, ProviderMetadata, ToolResultContentPart,
        ToolResultOutput,
    };

    fn prompt_with_content(content: Vec<Content>) -> Prompt {
        Prompt {
            model: "test-model".to_owned(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: vec![Message {
                role: Role::User,
                content,
            }],
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    #[test]
    fn projection_counts_top_level_and_tool_result_media() -> crate::Result<()> {
        let prompt = prompt_with_content(vec![
            Content::File {
                media_type: "image/png".to_owned(),
                data: DataContent::Url {
                    url: "https://example.invalid/image.png".to_owned(),
                },
                filename: None,
                provider_metadata: ProviderMetadata::new(),
            },
            Content::ToolResult {
                call_id: "call-1".to_owned(),
                tool_name: Some("inspect".to_owned()),
                output: ToolResultOutput::Content {
                    value: vec![
                        ToolResultContentPart::Text {
                            text: "safe text".to_owned(),
                        },
                        ToolResultContentPart::Media {
                            media_type: "audio/mpeg".to_owned(),
                            data: DataContent::Url {
                                url: "https://example.invalid/audio.mp3".to_owned(),
                            },
                        },
                        ToolResultContentPart::FileId {
                            media_type: Some("application/pdf".to_owned()),
                            id: "file-1".to_owned(),
                        },
                    ],
                },
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            },
        ]);
        let (fragments, coverage) = content_fragments(&prompt, 1024).map_err(|_| {
            crate::BitrouterError::internal("bounded request-check projection failed")
        })?;

        assert_eq!(fragments.len(), 1);
        assert_eq!(fragments[0].text.as_deref(), Some("safe text"));
        assert_eq!(coverage.text_fragments, 1);
        assert_eq!(coverage.excluded_media_fragments, 3);
        assert_eq!(
            coverage.status,
            RequestCheckCoverageStatus::CompleteWithinScope
        );
        Ok(())
    }

    #[test]
    fn projection_caps_empty_fragments() -> crate::Result<()> {
        let at_limit = prompt_with_content(
            (0..MAX_PROJECTED_FRAGMENTS)
                .map(|_| Content::Text {
                    text: String::new(),
                    provider_metadata: ProviderMetadata::new(),
                })
                .collect(),
        );
        let (fragments, coverage) = content_fragments(&at_limit, 0).map_err(|_| {
            crate::BitrouterError::internal("fragment ceiling rejected its exact boundary")
        })?;
        assert_eq!(fragments.len() as u64, MAX_PROJECTED_FRAGMENTS);
        assert_eq!(coverage.text_fragments, MAX_PROJECTED_FRAGMENTS);

        let over_limit = prompt_with_content(
            (0..=MAX_PROJECTED_FRAGMENTS)
                .map(|_| Content::Text {
                    text: String::new(),
                    provider_metadata: ProviderMetadata::new(),
                })
                .collect(),
        );
        let rejected = content_fragments(&over_limit, 0).err().ok_or_else(|| {
            crate::BitrouterError::internal("fragment ceiling admitted excess input")
        })?;
        assert_eq!(
            rejected.text_fragments,
            MAX_PROJECTED_FRAGMENTS.saturating_add(1)
        );
        assert_eq!(rejected.status, RequestCheckCoverageStatus::InputTooLarge);
        Ok(())
    }

    #[test]
    fn projection_bounds_json_serialization_by_remaining_bytes() -> crate::Result<()> {
        let json_output = |value| {
            prompt_with_content(vec![Content::ToolResult {
                call_id: "call-json".to_owned(),
                tool_name: Some("inspect".to_owned()),
                output: ToolResultOutput::Json { value },
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }])
        };
        let exact = json_output(serde_json::json!("123456"));
        let (fragments, coverage) = content_fragments(&exact, 8).map_err(|_| {
            crate::BitrouterError::internal("bounded JSON rejected its exact byte boundary")
        })?;
        assert_eq!(fragments[0].text.as_deref(), Some("\"123456\""));
        assert_eq!(coverage.text_bytes, 8);

        let oversized = json_output(serde_json::json!("x".repeat(1024 * 1024)));
        let rejected = content_fragments(&oversized, 8)
            .err()
            .ok_or_else(|| crate::BitrouterError::internal("bounded JSON admitted excess input"))?;
        assert_eq!(rejected.text_bytes, 9);
        assert_eq!(rejected.text_fragments, 1);
        assert_eq!(rejected.status, RequestCheckCoverageStatus::InputTooLarge);
        Ok(())
    }
}
