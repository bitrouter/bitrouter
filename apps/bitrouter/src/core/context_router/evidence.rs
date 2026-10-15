//! Immutable evidence groups and derived, query-scoped representations.

use crate::storage::checkpoint::snapshot::context::evidence::{
    DerivedArtifact, EvidenceArtifact, EvidenceBlock, EvidenceExtract, EvidenceSource, SourceSpan,
};

use std::collections::{BTreeMap, BTreeSet};

use bitrouter_ai::types::{Content, Message};

use crate::storage::checkpoint::snapshot::context::{ContextStore, digest, invalid};
use bitrouter_ai::core::protocol::{CoreError, HarnessManifest};

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
}

/// Split only at settled boundaries. A multi-call batch and all of its results
/// form one group even when unrelated messages occur between them.
pub fn groups(messages: &[Message]) -> Result<Vec<Vec<Message>>, CoreError> {
    crate::gateway::model::history::validate_history(messages).map_err(invalid)?;
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
