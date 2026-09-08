//! Versioned, source-local native history projection. Raw evidence is retained
//! independently; a context transition never deletes an executed action.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::types::{EdgeKind, Harness, MAX_RECORDS, NodeKey, SourceFormat, StoredRecord};
use crate::eval::types::canonical_digest;

/// A context entry can refer to one item inside a compaction replacement.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextRef {
    pub record_id: String,
    pub pointer: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContextTransition {
    pub kind: EdgeKind,
    pub record_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Projection {
    pub node: NodeKey,
    pub raw_record_ids: Vec<String>,
    pub effective_context: Vec<ContextRef>,
    pub transitions: Vec<ContextTransition>,
    pub producer_versions: BTreeSet<String>,
    pub gaps: BTreeSet<String>,
}

/// Only one native source/generation is projected at a time. ACP and native
/// replay are separate observations and must never be concatenated as turns.
pub struct Projector {
    projection: Projection,
    format: SourceFormat,
    source: Option<(String, String)>,
    next_sequence: Option<u64>,
    claude_messages: BTreeMap<String, ClaudeEntry>,
    claude_compactions: Vec<Value>,
    retained_bytes: usize,
    identity_observed: bool,
    inherited: bool,
    codex_turn_starts: Vec<usize>,
    codex_ordinals: Option<bool>,
    codex_next_ordinal: u64,
    codex_copied_history_end: Option<u64>,
}

struct ClaudeEntry {
    parent: Option<String>,
    reference: ContextRef,
    digest: String,
    kind: String,
    sequence: u64,
    main: bool,
    message_id: Option<String>,
    tool_result: bool,
    timestamp: String,
}

impl Projector {
    pub fn new(node: NodeKey, format: SourceFormat) -> Result<Self> {
        node.validate()?;
        ensure!(
            matches!(
                (node.harness, format),
                (Harness::Codex, SourceFormat::CodexRollout)
                    | (Harness::ClaudeCode, SourceFormat::ClaudeTranscript)
            ),
            "effective history requires a native transcript source"
        );
        Ok(Self {
            projection: Projection {
                node,
                raw_record_ids: vec![],
                effective_context: vec![],
                transitions: vec![],
                producer_versions: BTreeSet::new(),
                gaps: BTreeSet::new(),
            },
            format,
            source: None,
            next_sequence: None,
            claude_messages: BTreeMap::new(),
            claude_compactions: vec![],
            retained_bytes: 0,
            identity_observed: false,
            inherited: false,
            codex_turn_starts: vec![],
            codex_ordinals: None,
            codex_next_ordinal: 0,
            codex_copied_history_end: None,
        })
    }

    /// The caller must supply the already bounded parent checkpoint. Missing
    /// dependency evidence remains a gap until a complete projection is rebuilt.
    pub fn inherit(&mut self, parent: &Projection) -> Result<()> {
        ensure!(
            self.projection.raw_record_ids.is_empty(),
            "inheritance must precede child records"
        );
        ensure!(
            parent.node.harness == self.projection.node.harness
                && parent.node.namespace == self.projection.node.namespace,
            "foreign inherited context"
        );
        self.projection.effective_context = parent.effective_context.clone();
        self.inherited = true;
        self.projection.gaps.extend(parent.gaps.iter().cloned());
        Ok(())
    }

    pub fn push(&mut self, record: &StoredRecord) -> Result<()> {
        ensure!(
            self.projection.raw_record_ids.len() < MAX_RECORDS,
            "projection exceeds record limit"
        );
        let source = (record.source_id.clone(), record.input.generation.clone());
        if let Some(expected) = &self.source {
            ensure!(
                expected == &source,
                "projection mixes sources or generations"
            );
        } else {
            self.source = Some(source);
        }
        if let Some(next) = self.next_sequence {
            ensure!(
                record.input.sequence == next,
                "projection source sequence gap"
            );
        } else if record.input.sequence != 0 {
            self.gap("history_prefix_missing");
        }
        self.next_sequence = Some(record.input.sequence + 1);
        self.projection.raw_record_ids.push(record.id.clone());
        if let Some(version) = &record.input.producer_version {
            self.projection.producer_versions.insert(version.clone());
        }
        match self.format {
            SourceFormat::CodexRollout => self.codex(record)?,
            SourceFormat::ClaudeTranscript => self.claude(record)?,
            _ => anyhow::bail!("unsupported effective-history source"),
        }
        Ok(())
    }

    pub fn finish(mut self) -> Projection {
        if self.format == SourceFormat::ClaudeTranscript {
            self.finish_claude();
        }
        if !self.identity_observed {
            self.gap("native_identity_unverified");
        }
        if self.projection.producer_versions.is_empty() {
            self.gap("producer_version_unknown");
        }
        self.projection
    }

    fn gap(&mut self, gap: &str) {
        self.projection.gaps.insert(gap.into());
    }

    fn transition(&mut self, kind: EdgeKind, record: &StoredRecord) {
        self.projection.transitions.push(ContextTransition {
            kind,
            record_id: record.id.clone(),
        });
    }

    // Rollout is an internal format. App Server is the public lifecycle API:
    // https://learn.chatgpt.com/docs/app-server
    // The collector preserves unknown records and withholds complete coverage.
    fn codex(&mut self, record: &StoredRecord) -> Result<()> {
        let raw = &record.input.raw;
        let ordinal = raw.get("ordinal").and_then(Value::as_u64);
        if self.codex_ordinals.is_none() {
            self.codex_ordinals = Some(ordinal.is_some());
            self.codex_next_ordinal = raw
                .pointer("/payload/history_base/end_ordinal_exclusive")
                .and_then(Value::as_u64)
                .unwrap_or(0);
        }
        if ordinal.is_some() != self.codex_ordinals.unwrap_or(false)
            || (raw.get("ordinal").is_some() && ordinal.is_none())
        {
            self.gap("native_ordinal_mode_changed");
        }
        if let Some(ordinal) = ordinal {
            if ordinal != self.codex_next_ordinal {
                self.gap("native_ordinal_gap");
            }
            self.codex_next_ordinal = ordinal.saturating_add(1);
        }
        let payload = raw
            .get("payload")
            .context("Codex rollout record has no payload")?;
        match raw.get("type").and_then(Value::as_str) {
            Some("session_meta") => {
                if record.input.sequence == 0 {
                    self.codex_copied_history_end = payload
                        .get("subagent_history_start_ordinal")
                        .filter(|value| !value.is_null())
                        .map(|value| value.as_u64().context("invalid subagent history cut"))
                        .transpose()?;
                } else if self
                    .codex_copied_history_end
                    .zip(ordinal)
                    .is_some_and(|(cut, ordinal)| ordinal < cut)
                {
                    // Full-history subagents copy ancestor metadata as context.
                    // Only the source's first metadata record identifies its owner.
                    // https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/protocol/src/protocol.rs
                    return Ok(());
                }
                self.identity_observed = true;
                ensure!(
                    payload.get("id").and_then(Value::as_str)
                        == Some(self.projection.node.native_id.as_str()),
                    "Codex rollout thread mismatch"
                );
                if let Some(version) = payload.get("cli_version").and_then(Value::as_str) {
                    self.projection.producer_versions.insert(version.into());
                }
                if payload
                    .get("history_base")
                    .is_some_and(|value| !value.is_null())
                    && !self.inherited
                {
                    self.gap("inherited_history_missing");
                }
                if payload
                    .get("forked_from_id")
                    .is_some_and(|value| !value.is_null())
                {
                    self.transition(EdgeKind::Fork, record);
                }
            }
            Some("response_item") => {
                ensure!(
                    self.projection.effective_context.len() < MAX_RECORDS,
                    "context reference limit"
                );
                self.projection.effective_context.push(ContextRef {
                    record_id: record.id.clone(),
                    pointer: "/payload".into(),
                });
            }
            Some("compacted") => {
                self.transition(EdgeKind::Compact, record);
                self.codex_turn_starts.clear();
                if let Some(replacement) =
                    payload.get("replacement_history").and_then(Value::as_array)
                {
                    ensure!(
                        replacement.len() <= MAX_RECORDS,
                        "compaction reference limit"
                    );
                    self.projection.effective_context = replacement
                        .iter()
                        .enumerate()
                        .map(|(index, _)| ContextRef {
                            record_id: record.id.clone(),
                            pointer: format!("/payload/replacement_history/{index}"),
                        })
                        .collect();
                } else {
                    self.gap("compaction_replacement_unavailable");
                    self.projection.effective_context.clear();
                }
            }
            Some("event_msg") => match payload.get("type").and_then(Value::as_str) {
                Some("task_started") => self
                    .codex_turn_starts
                    .push(self.projection.effective_context.len()),
                Some("thread_rolled_back") => {
                    self.transition(EdgeKind::Rewind, record);
                    if let Some(turns) = payload.get("num_turns").and_then(Value::as_u64)
                        && let Ok(turns) = usize::try_from(turns)
                        && turns > 0
                        && turns <= self.codex_turn_starts.len()
                    {
                        let index = self.codex_turn_starts.len() - turns;
                        self.projection
                            .effective_context
                            .truncate(self.codex_turn_starts[index]);
                        self.codex_turn_starts.truncate(index);
                    } else {
                        self.gap("rollback_context_unavailable");
                    }
                }
                _ => {}
            },
            Some("turn_context") => {}
            _ => self.gap("unknown_rollout_record"),
        }
        Ok(())
    }

    // Source records use camelCase; SDK wire compact events use snake_case.
    // Preserve UUID-less entries in raw storage; do not content-hash dedup them.
    // https://code.claude.com/docs/en/agent-sdk/session-storage
    // https://code.claude.com/docs/en/agent-sdk/typescript#sdkcompactboundarymessage
    fn claude(&mut self, record: &StoredRecord) -> Result<()> {
        let raw = &record.input.raw;
        let session = raw.get("sessionId").and_then(Value::as_str);
        let agent = raw.get("agentId").and_then(Value::as_str);
        if let Some(session) = session {
            ensure!(
                session == self.projection.node.native_id,
                "Claude transcript session mismatch"
            );
        }
        if let Some(agent) = agent {
            ensure!(
                self.projection.node.agent_id.as_deref() == Some(agent),
                "Claude transcript agent mismatch"
            );
        }
        if session.is_some() && (self.projection.node.agent_id.is_none() || agent.is_some()) {
            self.identity_observed = true;
        }
        if let Some(version) = raw.get("version").and_then(Value::as_str) {
            super::types::identifier(version)?;
            self.projection.producer_versions.insert(version.into());
        }
        let kind = raw.get("type").and_then(Value::as_str).unwrap_or_default();
        let compact = raw.get("subtype").and_then(Value::as_str) == Some("compact_boundary");
        let Some(uuid) = raw.get("uuid").and_then(Value::as_str) else {
            if matches!(kind, "user" | "assistant") {
                self.gap("message_uuid_missing");
            }
            return Ok(());
        };
        if !matches!(
            kind,
            "user" | "assistant" | "system" | "progress" | "attachment"
        ) {
            return Ok(());
        }
        super::types::identifier(uuid)?;
        let parent = raw
            .get("parentUuid")
            .and_then(Value::as_str)
            .map(str::to_owned);
        let message_id = (kind == "assistant")
            .then(|| raw.pointer("/message/id").and_then(Value::as_str))
            .flatten()
            .map(str::to_owned);
        for id in [&parent, &message_id].into_iter().flatten() {
            super::types::identifier(id)?;
        }
        let timestamp = raw
            .get("timestamp")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_owned();
        ensure!(timestamp.len() <= 64, "native timestamp exceeds limit");
        // Claude Code 2.1.220 rewrites preserved UUIDs with a new promptId
        // and session slug during compaction. These describe the observation,
        // not a new message. Keep all original records; compare every other
        // field, including content, ancestry and compact metadata. The SDK's
        // session loader also indexes the latest occurrence of each UUID:
        // https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.257
        let message: BTreeMap<_, _> = raw
            .as_object()
            .context("Claude transcript record is not an object")?
            .iter()
            .filter(|(key, _)| !matches!(key.as_str(), "promptId" | "slug"))
            .collect();
        let digest = canonical_digest(&message)?;
        if let Some(previous) = self.claude_messages.get_mut(uuid) {
            if previous.digest != digest {
                self.gap("conflicting_message_uuid");
            } else {
                // The SDK's UUID map retains the last occurrence for leaf
                // selection, even when replayed payload bytes are identical.
                previous.sequence = record.input.sequence;
                previous.reference.record_id = record.id.clone();
                return Ok(());
            }
        }
        self.retained_bytes += uuid.len() * 2
            + parent.as_ref().map_or(0, String::len)
            + message_id.as_ref().map_or(0, String::len)
            + timestamp.len()
            + 512;
        ensure!(
            self.retained_bytes <= super::types::MAX_OBJECT_BYTES,
            "native graph byte limit"
        );
        let tool_result = kind == "user"
            && raw
                .pointer("/message/content")
                .and_then(Value::as_array)
                .is_some_and(|blocks| {
                    blocks.iter().any(|block| {
                        block.get("type").and_then(Value::as_str) == Some("tool_result")
                    })
                });
        let main = raw.get("isSidechain") != Some(&Value::Bool(true))
            && raw.get("isMeta") != Some(&Value::Bool(true))
            && raw.get("teamName").is_none_or(|value| value.is_null());
        self.claude_messages.insert(
            uuid.into(),
            ClaudeEntry {
                parent,
                reference: ContextRef {
                    record_id: record.id.clone(),
                    pointer: String::new(),
                },
                digest,
                kind: kind.into(),
                sequence: record.input.sequence,
                main,
                message_id,
                tool_result,
                timestamp,
            },
        );
        if compact {
            self.transition(EdgeKind::Compact, record);
            if let Some(metadata) = raw.get("compactMetadata") {
                self.retained_bytes += serde_json::to_vec(metadata)?.len();
                ensure!(
                    self.retained_bytes <= super::types::MAX_OBJECT_BYTES
                        && self.claude_compactions.len() < super::types::MAX_GRAPH_ITEMS,
                    "compaction graph limit"
                );
                self.claude_compactions.push(metadata.clone());
            } else {
                self.gap("compact_metadata_unavailable");
            }
        }
        Ok(())
    }

    fn finish_claude(&mut self) {
        // Rebuild after reading the complete checkpoint: summaries and later
        // children can appear after the compact boundary. Native keys differ
        // from SDK wire keys. Contract verified against the published SDK's
        // session message loader: https://www.npmjs.com/package/@anthropic-ai/claude-agent-sdk/v/0.3.232
        for metadata in std::mem::take(&mut self.claude_compactions) {
            if let Err(error) = self.preserve_claude(&metadata) {
                tracing::debug!(%error, "native compaction projection incomplete");
                self.gap("preserved_context_unavailable");
            }
        }
        let parents: BTreeSet<_> = self
            .claude_messages
            .values()
            .filter_map(|entry| entry.parent.as_deref())
            .collect();
        let mut candidates = BTreeSet::new();
        for (uuid, _) in self
            .claude_messages
            .iter()
            .filter(|(uuid, _)| !parents.contains(uuid.as_str()))
        {
            let mut current = Some(uuid.as_str());
            let mut seen = BTreeSet::new();
            while let Some(id) = current {
                if !seen.insert(id) {
                    break;
                }
                let Some(entry) = self.claude_messages.get(id) else {
                    break;
                };
                if matches!(entry.kind.as_str(), "user" | "assistant") {
                    candidates.insert(id.to_owned());
                    break;
                }
                current = entry.parent.as_deref();
            }
        }
        let prefer_main = candidates
            .iter()
            .any(|uuid| self.claude_messages[uuid].main);
        let leaf = candidates
            .into_iter()
            .filter(|uuid| !prefer_main || self.claude_messages[uuid].main)
            .max_by_key(|uuid| self.claude_messages[uuid].sequence);
        let mut chain = vec![];
        let mut visited = BTreeSet::new();
        let mut current = leaf;
        while let Some(uuid) = current {
            if !visited.insert(uuid.clone()) {
                self.gap("context_parent_cycle");
                break;
            }
            let Some(entry) = self.claude_messages.get(&uuid) else {
                self.gap("context_parent_missing");
                break;
            };
            current = entry.parent.clone();
            chain.push(uuid);
        }
        chain.reverse();
        // One assistant message may be persisted as sibling content-block
        // records. Restore all siblings and their tool-result children once.
        let mut last_message = BTreeMap::new();
        for uuid in &chain {
            if let Some(id) = &self.claude_messages[uuid].message_id {
                last_message.insert(id.clone(), uuid.clone());
            }
        }
        let mut siblings_by_message = BTreeMap::<String, Vec<String>>::new();
        let mut results_by_parent = BTreeMap::<String, Vec<String>>::new();
        for (uuid, entry) in &self.claude_messages {
            if let Some(message) = &entry.message_id {
                siblings_by_message
                    .entry(message.clone())
                    .or_default()
                    .push(uuid.clone());
            }
            if entry.tool_result
                && let Some(parent) = &entry.parent
            {
                results_by_parent
                    .entry(parent.clone())
                    .or_default()
                    .push(uuid.clone());
            }
        }
        let mut additions = BTreeMap::<String, Vec<String>>::new();
        for (message, anchor) in last_message {
            let siblings = siblings_by_message
                .get(&message)
                .map(Vec::as_slice)
                .unwrap_or_default();
            let mut blocks: Vec<_> = siblings
                .iter()
                .filter(|uuid| !visited.contains(*uuid))
                .cloned()
                .collect();
            let mut results: Vec<_> = siblings
                .iter()
                .filter_map(|uuid| results_by_parent.get(uuid))
                .flatten()
                .filter(|uuid| !visited.contains(*uuid))
                .cloned()
                .collect();
            for group in [&mut blocks, &mut results] {
                group.sort_by(|left, right| {
                    self.claude_messages[left]
                        .timestamp
                        .cmp(&self.claude_messages[right].timestamp)
                        .then(
                            self.claude_messages[left]
                                .sequence
                                .cmp(&self.claude_messages[right].sequence),
                        )
                });
            }
            blocks.extend(results);
            visited.extend(blocks.iter().cloned());
            additions.insert(anchor, blocks);
        }
        for uuid in chain {
            self.projection
                .effective_context
                .push(self.claude_messages[&uuid].reference.clone());
            if let Some(extra) = additions.remove(&uuid) {
                self.projection.effective_context.extend(
                    extra
                        .into_iter()
                        .map(|id| self.claude_messages[&id].reference.clone()),
                );
            }
        }
        if self.projection.effective_context.is_empty() && !self.claude_messages.is_empty() {
            self.gap("context_leaf_unavailable");
        }
    }

    fn preserve_claude(&mut self, metadata: &Value) -> Result<()> {
        ensure!(
            metadata.get("preserved_messages").is_none()
                && metadata.get("preserved_segment").is_none(),
            "SDK wire metadata cannot substitute for native compact metadata"
        );
        let (head, tail, anchor) = if let Some(messages) = metadata.get("preservedMessages") {
            let anchor = messages
                .get("anchorUuid")
                .and_then(Value::as_str)
                .context("invalid preserved anchor")?;
            super::types::identifier(anchor)?;
            let uuids = messages
                .get("uuids")
                .and_then(Value::as_array)
                .context("invalid preserved messages")?;
            ensure!(
                !uuids.is_empty() && uuids.len() <= MAX_RECORDS,
                "invalid preserved message count"
            );
            let mut ids = vec![];
            let mut unique = BTreeSet::new();
            for value in uuids {
                let uuid = value.as_str().context("invalid preserved UUID")?;
                super::types::identifier(uuid)?;
                ensure!(
                    unique.insert(uuid) && self.claude_messages.contains_key(uuid),
                    "missing or duplicate preserved UUID"
                );
                ids.push(uuid);
            }
            let mut previous = anchor.to_owned();
            for uuid in &ids {
                self.claude_messages
                    .get_mut(*uuid)
                    .context("missing preserved entry")?
                    .parent = Some(previous);
                previous = (*uuid).into();
            }
            (ids[0].to_owned(), previous, anchor.to_owned())
        } else if let Some(segment) = metadata.get("preservedSegment") {
            let head = segment
                .get("headUuid")
                .and_then(Value::as_str)
                .context("invalid preserved head")?;
            let tail = segment
                .get("tailUuid")
                .and_then(Value::as_str)
                .context("invalid preserved tail")?;
            let anchor = segment
                .get("anchorUuid")
                .and_then(Value::as_str)
                .context("invalid preserved anchor")?;
            for id in [head, tail, anchor] {
                super::types::identifier(id)?;
            }
            let mut current = Some(tail.to_owned());
            let mut seen = BTreeSet::new();
            while let Some(uuid) = current {
                ensure!(seen.insert(uuid.clone()), "preserved segment cycle");
                if uuid == head {
                    break;
                }
                current = self
                    .claude_messages
                    .get(&uuid)
                    .and_then(|entry| entry.parent.clone());
            }
            ensure!(seen.contains(head), "preserved segment missing");
            self.claude_messages
                .get_mut(head)
                .context("preserved head missing")?
                .parent = Some(anchor.into());
            (head.into(), tail.into(), anchor.into())
        } else {
            return Ok(());
        };
        ensure!(
            self.claude_messages.contains_key(&anchor),
            "preserved anchor missing"
        );
        for (uuid, entry) in &mut self.claude_messages {
            if entry.parent.as_ref() == Some(&anchor) && uuid != &head {
                entry.parent = Some(tail.clone());
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests;
