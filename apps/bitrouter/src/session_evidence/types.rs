//! Versioned identities and immutable evidence checkpoints.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Result, ensure};
use serde::{Deserialize, Serialize};

use crate::eval::types::{EvalDecisionRef, canonical_digest};

pub const SCHEMA_VERSION: u32 = 1;
pub const PARSER_VERSION: &str = "native-evidence/1";
pub const MAX_RECORD_BYTES: usize = 32 * 1024 * 1024;
pub const MAX_OBJECT_BYTES: usize = 64 * 1024 * 1024;
pub const MAX_RECORDS: usize = 100_000;
pub const MAX_GRAPH_ITEMS: usize = 1024;
// At most two 32 MiB raw records are materialized by one database read.
pub const RECORD_PAGE_SIZE: u64 = 2;

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Harness {
    Codex,
    ClaudeCode,
}

/// A native execution node. Grouping, spawn ancestry and forks are separate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NodeKey {
    pub namespace: String,
    pub harness: Harness,
    /// Codex thread id or Claude session id. Codex session-tree grouping is
    /// mutable metadata and is deliberately excluded from execution identity.
    pub native_id: String,
    pub agent_id: Option<String>,
}

impl NodeKey {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.namespace)?;
        identifier(&self.native_id)?;
        if let Some(id) = &self.agent_id {
            identifier(id)?;
        }
        ensure!(
            self.harness != Harness::Codex || self.agent_id.is_none(),
            "a Codex child has its own thread identity"
        );
        Ok(())
    }

    /// Codex tree/session grouping is mutable runtime metadata. The thread is
    /// the execution identity, including when another frontend adopts a fork.
    pub fn id(&self) -> Result<String> {
        self.validate()?;
        canonical_digest(self)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceFormat {
    CodexRollout,
    CodexAppServer,
    ClaudeTranscript,
    ClaudeHook,
    Acp,
}

/// Only explicitly registered sources are collected. A locator is metadata,
/// never authority to open an arbitrary path supplied by a transcript.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceDescriptor {
    pub namespace: String,
    pub harness: Harness,
    pub format: SourceFormat,
    pub locator: String,
    pub node: Option<NodeKey>,
}

impl SourceDescriptor {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.namespace)?;
        ensure!(!self.locator.is_empty(), "evidence source locator is empty");
        ensure!(
            self.locator.len() <= 8192,
            "evidence source locator is too long"
        );
        if let Some(node) = &self.node {
            node.validate()?;
            ensure!(
                node.namespace == self.namespace,
                "source namespace mismatch"
            );
            ensure!(node.harness == self.harness, "source harness mismatch");
        }
        Ok(())
    }

    pub fn id(&self, owner: &str) -> Result<String> {
        identifier(owner)?;
        self.validate()?;
        canonical_digest(&(owner, self))
    }
}

/// The cursor is committed atomically with all records preceding it. A new
/// generation distinguishes replacement/truncation from append and replay.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceCursor {
    pub generation: String,
    pub offset: u64,
    pub next_sequence: u64,
    pub anchor_digest: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RegisteredSource {
    pub id: String,
    pub descriptor: SourceDescriptor,
    pub revision: i64,
    pub cursor: SourceCursor,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct RecordInput {
    pub generation: String,
    pub sequence: u64,
    /// Exact native source byte interval, including the terminating newline.
    /// Live protocol observations do not claim file positions.
    pub byte_start: Option<u64>,
    pub byte_end: Option<u64>,
    pub producer_version: Option<String>,
    pub raw: serde_json::Value,
}

impl RecordInput {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.generation)?;
        ensure!(
            matches!((self.byte_start, self.byte_end), (None, None))
                || matches!((self.byte_start, self.byte_end), (Some(start), Some(end)) if start < end),
            "invalid native record byte interval"
        );
        if let Some(version) = &self.producer_version {
            identifier(version)?;
        }
        ensure!(self.sequence <= i64::MAX as u64, "source sequence overflow");
        ensure!(
            self.raw.is_object(),
            "native evidence record must be an object"
        );
        ensure!(
            serde_json::to_vec(&self.raw)?.len() <= MAX_RECORD_BYTES,
            "native evidence record exceeds the size limit"
        );
        Ok(())
    }

    pub fn id(&self, source_id: &str) -> Result<String> {
        canonical_digest(&(source_id, &self.generation, self.sequence))
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct StoredRecord {
    pub id: String,
    pub source_id: String,
    pub digest: String,
    pub input: RecordInput,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Completeness {
    Complete,
    Partial,
    Unknown,
    Unavailable,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Coverage {
    pub history: Completeness,
    pub lineage: Completeness,
    pub requests: Completeness,
    pub artifacts: Completeness,
    pub gaps: BTreeSet<String>,
}

impl Default for Coverage {
    fn default() -> Self {
        Self {
            history: Completeness::Unknown,
            lineage: Completeness::Unknown,
            requests: Completeness::Unknown,
            artifacts: Completeness::Unknown,
            gaps: BTreeSet::new(),
        }
    }
}

impl Coverage {
    pub fn optimization_eligible(&self) -> bool {
        [self.history, self.lineage, self.requests, self.artifacts]
            .into_iter()
            .all(|value| value == Completeness::Complete)
            && self.gaps.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EdgeKind {
    Spawn,
    Fork,
    Resume,
    Compact,
    Rewind,
    Message,
}

/// The range is exclusive at the end and immutable once referenced by a fork.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SourceRange {
    pub source_id: String,
    pub generation: String,
    pub start: u64,
    pub end: u64,
}

impl SourceRange {
    pub fn validate(&self) -> Result<()> {
        digest_identifier(&self.source_id)?;
        identifier(&self.generation)?;
        ensure!(
            self.start < self.end && self.end <= i64::MAX as u64,
            "invalid source range"
        );
        ensure!(
            self.end - self.start <= MAX_RECORDS as u64,
            "source range is too large"
        );
        Ok(())
    }

    pub fn contains(&self, other: &Self) -> bool {
        self.source_id == other.source_id
            && self.generation == other.generation
            && self.start <= other.start
            && self.end >= other.end
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionEdge {
    pub kind: EdgeKind,
    pub from: NodeKey,
    pub to: NodeKey,
    pub checkpoint: Option<SourceRange>,
    pub evidence_record_ids: BTreeSet<String>,
}

/// An immutable native history-base reference, bound to stored parent records.
/// The key survives child file generations because native fork ancestry does
/// not change when either file is compacted, moved, rewritten or removed.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ForkBinding {
    pub id: String,
    pub revision: u64,
    pub child: NodeKey,
    pub parent: NodeKey,
    pub end_ordinal_exclusive: u64,
    pub end_byte_offset: u64,
    pub child_record_id: String,
    pub child_record_digest: String,
    pub checkpoint: SourceRange,
    pub record_digests: BTreeMap<String, String>,
    /// Display provenance only; loading the binding never opens this path.
    pub observed_path: Option<std::path::PathBuf>,
}

impl ForkBinding {
    pub fn key(child: &NodeKey, parent: &NodeKey, ordinal: u64, bytes: u64) -> Result<String> {
        canonical_digest(&(child, parent, ordinal, bytes))
    }

    pub fn validate(&self) -> Result<()> {
        self.child.validate()?;
        self.parent.validate()?;
        self.checkpoint.validate()?;
        ensure!(self.revision == 0, "fork bindings are immutable");
        ensure!(
            self.child.harness == Harness::Codex
                && self.parent.harness == Harness::Codex
                && self.child.namespace == self.parent.namespace
                && self.child != self.parent,
            "invalid fork identity"
        );
        ensure!(
            self.id
                == Self::key(
                    &self.child,
                    &self.parent,
                    self.end_ordinal_exclusive,
                    self.end_byte_offset
                )?,
            "fork binding identity mismatch"
        );
        digest_identifier(&self.child_record_id)?;
        digest_identifier(&self.child_record_digest)?;
        ensure!(
            self.checkpoint.start == 0
                && self.end_byte_offset > 0
                && self.end_ordinal_exclusive > 0,
            "invalid fork prefix"
        );
        ensure!(
            self.record_digests.len() as u64 == self.checkpoint.end,
            "fork record set mismatch"
        );
        ensure!(
            self.observed_path
                .as_ref()
                .is_none_or(|path| path.as_os_str().len() <= 8192),
            "fork provenance path limit"
        );
        for (id, digest) in &self.record_digests {
            digest_identifier(id)?;
            digest_identifier(digest)?;
        }
        Ok(())
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AttemptPhase {
    Collecting,
    Settling,
    Ready,
    Partial,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Attempt {
    pub id: String,
    pub task_id: String,
    pub root: NodeKey,
    pub members: BTreeSet<NodeKey>,
    pub phase: AttemptPhase,
    pub revision: u64,
    pub latest_manifest: Option<String>,
    pub effective_manifest: Option<String>,
    pub started_at: String,
}

impl Attempt {
    pub fn validate(&self) -> Result<()> {
        identifier(&self.id)?;
        identifier(&self.task_id)?;
        self.root.validate()?;
        ensure!(
            self.members.len() <= MAX_GRAPH_ITEMS,
            "too many attempt members"
        );
        timestamp(&self.started_at)?;
        for digest in [&self.latest_manifest, &self.effective_manifest]
            .into_iter()
            .flatten()
        {
            digest_identifier(digest)?;
        }
        ensure!(
            self.members.contains(&self.root),
            "attempt must include its root"
        );
        for member in &self.members {
            member.validate()?;
            ensure!(
                member.namespace == self.root.namespace,
                "attempt namespace mismatch"
            );
            ensure!(
                member.harness == self.root.harness,
                "attempt harness mismatch"
            );
        }
        ensure!(
            self.revision <= i64::MAX as u64,
            "attempt revision overflow"
        );
        Ok(())
    }
}

/// A local artifact is identified by its content, not a mutable working path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Artifact {
    pub kind: String,
    pub digest: String,
    pub content: String,
    pub attributes: BTreeMap<String, String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct EvidenceManifest {
    pub schema_version: u32,
    pub parser_version: String,
    pub attempt_id: String,
    pub revision: u64,
    pub members: BTreeSet<NodeKey>,
    pub ranges: Vec<SourceRange>,
    pub record_digests: BTreeMap<String, String>,
    pub edges: Vec<ExecutionEdge>,
    pub coverage: Coverage,
    pub request_ids: BTreeSet<String>,
    pub decisions: Vec<EvalDecisionRef>,
    /// Decision id to the actual gateway request id; request_key on an Eval
    /// decision is a routing classification and is not an accounting id.
    pub decision_requests: BTreeMap<String, String>,
    pub artifacts: Vec<Artifact>,
    pub frozen_at: String,
}

impl EvidenceManifest {
    pub fn digest(&self) -> Result<String> {
        canonical_digest(self)
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            self.schema_version == SCHEMA_VERSION,
            "unknown evidence schema"
        );
        identifier(&self.attempt_id)?;
        identifier(&self.parser_version)?;
        timestamp(&self.frozen_at)?;
        ensure!(
            self.revision <= i64::MAX as u64,
            "manifest revision overflow"
        );
        ensure!(
            serde_json::to_vec(self)?.len() <= MAX_OBJECT_BYTES,
            "manifest exceeds size limit"
        );
        ensure!(
            self.members.len() <= MAX_GRAPH_ITEMS
                && self.ranges.len() <= MAX_GRAPH_ITEMS
                && self.edges.len() <= MAX_GRAPH_ITEMS
                && self.artifacts.len() <= MAX_GRAPH_ITEMS
                && self.coverage.gaps.len() <= MAX_GRAPH_ITEMS,
            "manifest graph exceeds size limit"
        );
        ensure!(
            self.record_digests.len() <= MAX_RECORDS
                && self.request_ids.len() <= MAX_RECORDS
                && self.decisions.len() <= MAX_RECORDS,
            "manifest evidence exceeds size limit"
        );
        for gap in &self.coverage.gaps {
            identifier(gap)?;
        }
        ensure!(!self.members.is_empty(), "manifest has no execution nodes");
        for member in &self.members {
            member.validate()?;
        }
        let mut ranges = BTreeMap::<(&str, &str), Vec<&SourceRange>>::new();
        let mut count = 0u64;
        for range in &self.ranges {
            range.validate()?;
            count += range.end - range.start;
            ranges
                .entry((&range.source_id, &range.generation))
                .or_default()
                .push(range);
        }
        ensure!(count <= MAX_RECORDS as u64, "too many referenced records");
        for group in ranges.values_mut() {
            group.sort_by_key(|range| range.start);
            ensure!(
                group.windows(2).all(|pair| pair[0].end <= pair[1].start),
                "overlapping source ranges"
            );
        }
        for (id, digest) in &self.record_digests {
            digest_identifier(id)?;
            digest_identifier(digest)?;
        }
        let mut edge_digests = BTreeSet::new();
        for edge in &self.edges {
            edge.from.validate()?;
            edge.to.validate()?;
            ensure!(
                edge.from.namespace == edge.to.namespace && edge.from.harness == edge.to.harness,
                "edge crosses native namespace"
            );
            ensure!(
                !edge.evidence_record_ids.is_empty()
                    && edge.evidence_record_ids.len() <= MAX_RECORDS,
                "edge requires bounded provenance"
            );
            ensure!(
                edge.evidence_record_ids
                    .iter()
                    .all(|id| self.record_digests.contains_key(id)),
                "edge evidence is outside the manifest"
            );
            if let Some(checkpoint) = &edge.checkpoint {
                checkpoint.validate()?;
                ensure!(
                    self.ranges.iter().any(|range| range.contains(checkpoint)),
                    "edge checkpoint is outside the manifest"
                );
            }
            ensure!(
                edge.kind != EdgeKind::Fork || edge.checkpoint.is_some(),
                "fork requires a checkpoint"
            );
            ensure!(
                edge_digests.insert(canonical_digest(edge)?),
                "duplicate execution edge"
            );
        }
        let mut artifact_digests = BTreeSet::new();
        for artifact in &self.artifacts {
            identifier(&artifact.kind)?;
            ensure!(
                artifact.content.len() <= MAX_RECORD_BYTES
                    && artifact.attributes.len() <= MAX_GRAPH_ITEMS,
                "artifact exceeds size limit"
            );
            for (key, value) in &artifact.attributes {
                identifier(key)?;
                identifier(value)?;
            }
            ensure!(
                canonical_digest(&artifact.content)? == artifact.digest,
                "artifact content digest mismatch"
            );
            ensure!(
                artifact_digests.insert(canonical_digest(artifact)?),
                "duplicate artifact"
            );
        }
        for id in &self.request_ids {
            identifier(id)?;
        }
        ensure!(
            self.decision_requests.len() == self.decisions.len(),
            "decision request bindings mismatch"
        );
        let mut decision_ids = BTreeSet::new();
        if self.coverage.optimization_eligible() {
            ensure!(
                !self.ranges.is_empty()
                    && !self.record_digests.is_empty()
                    && !self.request_ids.is_empty()
                    && !self.decisions.is_empty()
                    && !self.artifacts.is_empty(),
                "optimization requires execution, routing and artifact evidence"
            );
        }
        for decision in &self.decisions {
            identifier(&decision.decision_id)?;
            ensure!(
                decision_ids.insert(&decision.decision_id),
                "duplicate decision"
            );
            ensure!(
                self.decision_requests
                    .get(&decision.decision_id)
                    .is_some_and(|id| self.request_ids.contains(id)),
                "decision request is outside the manifest"
            );
        }
        Ok(())
    }
}

pub(crate) fn digest_identifier(value: &str) -> Result<()> {
    ensure!(
        value
            .strip_prefix("sha256:")
            .is_some_and(|hex| hex.len() == 64
                && hex
                    .bytes()
                    .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte))),
        "invalid evidence digest"
    );
    Ok(())
}

fn timestamp(value: &str) -> Result<()> {
    ensure!(value.len() <= 64, "invalid evidence timestamp");
    chrono::DateTime::parse_from_rfc3339(value)?;
    Ok(())
}

pub(crate) fn identifier(value: &str) -> Result<()> {
    ensure!(
        !value.trim().is_empty() && value.len() <= 512 && !value.chars().any(char::is_control),
        "invalid evidence identifier"
    );
    Ok(())
}
