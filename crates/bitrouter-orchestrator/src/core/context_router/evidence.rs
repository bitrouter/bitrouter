//! Immutable evidence groups and derived, query-scoped representations.

use std::collections::{BTreeMap, BTreeSet};

use bitrouter_ai::types::{Content, Message, Role, ToolResultOutput};
use serde::{Deserialize, Serialize};

use super::{ContextStore, digest, invalid};
use crate::core::protocol::{CoreError, HarnessManifest};

/// Provenance is immutable even if the workspace subsequently changes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvidenceSource {
    pub task_id: String,
    pub agent_id: String,
    pub workspace_id: String,
    pub workspace_revision: Option<String>,
    pub permission_revision: u64,
    pub tool_manifest_digest: String,
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
        }
    }

    /// Unknown versions do not prove current workspace material.
    pub fn is_current(&self, manifest: &HarnessManifest) -> bool {
        self.derivation_compatible(manifest) && self.workspace_revision.is_some()
    }

    /// Unknown workspace revisions permit a historical representation of the
    /// immutable source, without asserting that the file remains current.
    /// A changed admitted version or authority requires a new representation.
    pub(super) fn derivation_compatible(&self, manifest: &HarnessManifest) -> bool {
        self.workspace_id == manifest.workspace_id
            && self.permission_revision == manifest.permission_revision
            && self.tool_manifest_digest == manifest.tool_manifest_digest
            && self.workspace_revision == manifest.workspace_revision
    }
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

impl EvidenceBlock {
    pub fn new(messages: Vec<Message>, source: EvidenceSource) -> Result<Self, CoreError> {
        crate::context::validate_history(&messages).map_err(invalid)?;
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
    pub reference: crate::core::protocol::ArtifactRef,
    pub source: EvidenceSource,
}

impl EvidenceArtifact {
    pub fn visible(&self, store: &ContextStore, task_id: &str, manifest: &HarnessManifest) -> bool {
        self.source.workspace_id == manifest.workspace_id
            && self.source.permission_revision == manifest.permission_revision
            && store.work.get(task_id).is_some_and(|work| {
                self.source.task_id == work.task_id
                    || work.evidence.iter().any(|id| {
                        store
                            .evidence
                            .get(id)
                            .is_some_and(|block| block.source.task_id == self.source.task_id)
                    })
            })
    }
}

impl ContextStore {
    pub fn publish_extract(
        &mut self,
        task_id: &str,
        block_id: &str,
        spans: Vec<SourceSpan>,
    ) -> Result<String, CoreError> {
        let work = self
            .work
            .get(task_id)
            .ok_or_else(|| invalid("extract has no task"))?;
        if !work.evidence.iter().any(|id| id == block_id) || spans.len() > 16 {
            return Err(invalid(
                "extract source is outside the task or has too many spans",
            ));
        }
        self.extract(block_id, &spans)?;
        let task_sha256 = digest(&(&work.text, &work.acceptance_criteria, &work.instructions))?;
        let extract_id = format!("extract_{}", digest(&(&task_sha256, block_id, &spans))?);
        self.extracts.insert(
            extract_id.clone(),
            EvidenceExtract {
                extract_id: extract_id.clone(),
                block_id: block_id.into(),
                task_sha256,
                spans,
            },
        );
        Ok(extract_id)
    }
    pub fn insert_block(&mut self, block: EvidenceBlock) -> Result<String, CoreError> {
        block.validate()?;
        if self
            .evidence
            .get(&block.block_id)
            .is_some_and(|prior| prior != &block)
        {
            return Err(invalid("evidence identity collision"));
        }
        let id = block.block_id.clone();
        self.evidence.insert(id.clone(), block);
        Ok(id)
    }

    /// Preserve prior provenance for identical groups already owned by this task.
    /// A changed workspace revision must not relabel old tool output as fresh.
    pub fn capture(
        &mut self,
        messages: &[Message],
        source: &EvidenceSource,
    ) -> Result<Vec<String>, CoreError> {
        self.capture_with_sources(messages, source, &BTreeMap::new())
    }

    pub fn capture_with_sources(
        &mut self,
        messages: &[Message],
        source: &EvidenceSource,
        call_sources: &BTreeMap<String, EvidenceSource>,
    ) -> Result<Vec<String>, CoreError> {
        let prior: BTreeMap<String, String> = self
            .evidence
            .values()
            .filter(|block| {
                block.source.agent_id == source.agent_id
                    || self
                        .work
                        .get(&source.task_id)
                        .is_some_and(|work| work.evidence.contains(&block.block_id))
            })
            .map(|block| (block.content_sha256.clone(), block.block_id.clone()))
            .collect();
        let mut ids = Vec::new();
        for group in groups(messages)? {
            let content_sha256 = digest(&group)?;
            let id = match prior.get(&content_sha256) {
                Some(id) => id.clone(),
                None => {
                    let mut provenance = source.clone();
                    let calls: Vec<_> = group
                        .iter()
                        .flat_map(|message| &message.content)
                        .filter_map(|content| match content {
                            Content::ToolResult { call_id, .. } => Some(call_id),
                            _ => None,
                        })
                        .collect();
                    if !calls.is_empty() {
                        let first = calls.first().and_then(|id| call_sources.get(*id));
                        if let Some(first) = first.filter(|first| {
                            calls.iter().all(|id| call_sources.get(*id) == Some(*first))
                        }) {
                            provenance = first.clone();
                        } else {
                            provenance.workspace_revision = None;
                        }
                    }
                    self.insert_block(EvidenceBlock::new(group, provenance)?)?
                }
            };
            ids.push(id);
        }
        Ok(ids)
    }

    pub fn publish_summary(
        &mut self,
        task_id: &str,
        source_blocks: Vec<String>,
        text: String,
    ) -> Result<String, CoreError> {
        let work = self
            .work
            .get(task_id)
            .ok_or_else(|| invalid("summary has no task"))?;
        if text.is_empty()
            || text.len() > 16 * 1024
            || source_blocks.len() != 1
            || source_blocks.iter().any(|id| {
                !work.evidence.contains(id)
                    || self.evidence.get(id).is_none_or(|block| block.protected)
            })
        {
            return Err(invalid(
                "summary requires bounded text and one optional task evidence group",
            ));
        }
        let task_sha256 = digest(&(&work.text, &work.acceptance_criteria, &work.instructions))?;
        let artifact_id = format!(
            "summary_{}",
            digest(&(&task_sha256, &source_blocks, &text))?
        );
        self.derived.insert(
            artifact_id.clone(),
            DerivedArtifact {
                artifact_id: artifact_id.clone(),
                task_sha256,
                source_blocks,
                text,
            },
        );
        Ok(artifact_id)
    }

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

/// Split only at settled boundaries. A multi-call batch and all of its results
/// form one group even when unrelated messages occur between them.
pub fn groups(messages: &[Message]) -> Result<Vec<Vec<Message>>, CoreError> {
    crate::context::validate_history(messages).map_err(invalid)?;
    let mut pending = BTreeSet::new();
    let mut result = Vec::new();
    let mut group = Vec::new();
    for message in messages {
        group.push(message.clone());
        for content in &message.content {
            match content {
                Content::ToolCall {
                    id,
                    provider_executed: false,
                    ..
                } => {
                    pending.insert(id.clone());
                }
                Content::ToolResult { call_id, .. } => {
                    pending.remove(call_id);
                }
                _ => {}
            }
        }
        if pending.is_empty() {
            result.push(std::mem::take(&mut group));
        }
    }
    Ok(result)
}
