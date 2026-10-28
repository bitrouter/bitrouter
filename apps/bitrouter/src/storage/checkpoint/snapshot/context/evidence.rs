//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use super::{ContextStore, digest, invalid};
use bitrouter_ai::core::protocol::{CoreError, HarnessManifest};
use bitrouter_ai::types::{Content, Message, Role, ToolResultOutput};
use serde::{Deserialize, Serialize};

/// Provenance is immutable even if the workspace subsequently changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSource {
    pub task_id: String,
    pub agent_id: String,
    pub workspace_id: String,
    pub workspace_revision: Option<String>,
    pub permission_revision: u64,
    pub tool_manifest_digest: String,
    /// Host-bound MCP instruction data cannot inherit another task's consent.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skill_binding: Option<String>,
}

/// An atomic source group. Tool calls and their results are always stored together.
#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct EvidenceBlock {
    pub block_id: String,
    pub content_sha256: String,
    pub source: EvidenceSource,
    pub messages: Vec<Message>,
    pub bytes: usize,
    /// Instructions and opaque provider continuity cannot be rewritten or hidden.
    pub protected: bool,
}

/// Half-open byte spans, always UTF-8 aligned and tied to exact source text.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct SourceSpan {
    pub message_index: usize,
    pub content_index: usize,
    pub start: usize,
    pub end: usize,
}

/// A generation-authored summary. Sources remain independently recallable.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct DerivedArtifact {
    pub artifact_id: String,
    pub task_sha256: String,
    pub source_blocks: Vec<String>,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceExtract {
    pub extract_id: String,
    pub block_id: String,
    pub task_sha256: String,
    pub spans: Vec<SourceSpan>,
}

/// A complete tool body, retained independently of its bounded prompt preview.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceArtifact {
    pub reference: bitrouter_ai::core::protocol::ArtifactRef,
    pub source: EvidenceSource,
}

impl EvidenceSource {
    pub fn from_manifest(task_id: &str, agent_id: &str, manifest: &HarnessManifest) -> Self {
        Self {
            task_id: task_id.into(),
            agent_id: agent_id.into(),
            workspace_id: manifest.workspace_id.clone(),
            workspace_revision: manifest.workspace_revision.clone(),
            permission_revision: manifest.permission_revision,
            tool_manifest_digest: manifest.tool_manifest_digest.clone(),
            skill_binding: None,
        }
    }

    /// Unknown versions do not prove current workspace material.
    pub fn is_current(&self, manifest: &HarnessManifest) -> bool {
        self.derivation_compatible(manifest) && self.workspace_revision.is_some()
    }

    /// Unknown workspace revisions permit a historical representation of the
    /// immutable source, without asserting that the file remains current.
    /// A changed admitted version or authority requires a new representation.
    pub(crate) fn derivation_compatible(&self, manifest: &HarnessManifest) -> bool {
        self.workspace_id == manifest.workspace_id
            && self.permission_revision == manifest.permission_revision
            && self.tool_manifest_digest == manifest.tool_manifest_digest
            && self.workspace_revision == manifest.workspace_revision
    }
}

impl EvidenceBlock {
    pub fn new(messages: Vec<Message>, source: EvidenceSource) -> Result<Self, CoreError> {
        crate::gateway::model::history::validate_history(&messages).map_err(invalid)?;
        let content_sha256 = digest(&messages)?;
        let block_id = format!("block_{}", digest(&(&source, &content_sha256))?);
        let protected = messages.iter().any(|message| {
            matches!(message.role, Role::System | Role::User)
                || message.content.iter().any(protected_content)
        });
        let bytes = serde_json::to_vec(&messages)
            .map_err(|error| invalid(error.to_string()))?
            .len();
        Ok(Self {
            block_id,
            content_sha256,
            source,
            messages,
            bytes,
            protected,
        })
    }

    pub fn validate(&self) -> Result<(), CoreError> {
        let expected = Self::new(self.messages.clone(), self.source.clone())?;
        if *self != expected {
            return Err(invalid("immutable evidence commitment changed"));
        }
        Ok(())
    }
}

fn protected_content(content: &Content) -> bool {
    match content {
        Content::ToolCall {
            provider_executed: true,
            ..
        }
        | Content::ToolCall { dynamic: true, .. }
        | Content::ToolResult { dynamic: true, .. } => true,
        Content::Text {
            provider_metadata, ..
        }
        | Content::ToolCall {
            provider_metadata, ..
        }
        | Content::ToolResult {
            provider_metadata, ..
        } => !provider_metadata.is_empty(),
        // Reasoning and non-text parts can carry provider continuation semantics.
        _ => true,
    }
}

impl ContextStore {
    pub fn extract(&self, block_id: &str, spans: &[SourceSpan]) -> Result<String, CoreError> {
        let block = self
            .evidence
            .get(block_id)
            .ok_or_else(|| invalid("extract source is absent"))?;
        if block.protected || spans.is_empty() {
            return Err(invalid("extract source is protected or spans are empty"));
        }
        let mut text = String::new();
        for span in spans {
            let content = block
                .messages
                .get(span.message_index)
                .and_then(|message| message.content.get(span.content_index));
            let source = match content {
                Some(Content::Text { text, .. }) => text,
                Some(Content::ToolResult {
                    output: ToolResultOutput::Text { value } | ToolResultOutput::ErrorText { value },
                    ..
                }) => value,
                _ => return Err(invalid("extract span is not plain source text")),
            };
            if span.start >= span.end {
                return Err(invalid("empty or reversed extract span"));
            }
            let selected = source
                .get(span.start..span.end)
                .ok_or_else(|| invalid("extract span exceeds UTF-8 source boundaries"))?;
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(selected);
        }
        Ok(text)
    }
}
