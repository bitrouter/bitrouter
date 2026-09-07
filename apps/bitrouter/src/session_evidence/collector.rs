//! Reconcile explicitly registered native transcript roots into durable records.
//! Filesystem notifications may wake this collector, but are not its journal.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::fs::File;
use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncSeekExt, BufReader, SeekFrom};

use super::store::EvidenceStore;
use super::types::{
    Harness, MAX_GRAPH_ITEMS, MAX_RECORD_BYTES, MAX_RECORDS, NodeKey, RecordInput,
    RegisteredSource, SourceCursor, SourceDescriptor, SourceFormat, SourceRange,
};
use crate::eval::types::canonical_digest;

const MAX_SCAN_ENTRIES: usize = 100_000;
const MAX_SCAN_DEPTH: usize = 12;
const MAX_PREFIX_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Debug, Clone)]
pub struct NativeRoot {
    pub harness: Harness,
    pub namespace: String,
    pub directory: PathBuf,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CollectedSource {
    pub source: RegisteredSource,
    pub range: Option<SourceRange>,
    pub path: Option<PathBuf>,
    pub gaps: BTreeSet<String>,
    /// Codex's first rollout record, used for explicit bounded dependencies.
    pub metadata: Option<Value>,
}

/// A root is configured by the application, not by a model or transcript.
#[derive(Clone)]
pub struct NativeCollector {
    store: EvidenceStore,
    root: NativeRoot,
}

impl NativeCollector {
    pub fn new(store: EvidenceStore, root: NativeRoot) -> Result<Self> {
        super::types::identifier(&root.namespace)?;
        ensure!(root.directory.is_absolute(), "native root must be absolute");
        Ok(Self { store, root })
    }

    pub fn root(&self) -> &NativeRoot {
        &self.root
    }

    fn directories(&self) -> Vec<PathBuf> {
        let mut directories = vec![self.root.directory.clone()];
        if self.root.harness == Harness::Codex
            && self
                .root
                .directory
                .file_name()
                .is_some_and(|name| name == "sessions")
            && let Some(profile) = self.root.directory.parent()
        {
            // Codex thread/archive moves the same rollout into this directory.
            // Its native id and file identity do not change with that move.
            // https://learn.chatgpt.com/docs/app-server
            directories.push(profile.join("archived_sessions"));
        }
        directories
    }

    /// Discover only the named execution and explicitly requested Claude
    /// subagents. Search candidates by native filename, then verify identities
    /// from their contents before advancing any durable cursor.
    pub async fn discover(&self, native_id: &str) -> Result<Vec<(NodeKey, PathBuf)>> {
        safe_native_component(native_id)?;
        let mut pending = vec![];
        for directory in self.directories() {
            if tokio::fs::try_exists(&directory).await? {
                pending.push((tokio::fs::canonicalize(directory).await?, 0));
            }
        }
        let mut count = 0;
        let mut found = vec![];
        while let Some((directory, depth)) = pending.pop() {
            ensure!(
                depth <= MAX_SCAN_DEPTH,
                "native source tree exceeds depth limit"
            );
            let mut entries = tokio::fs::read_dir(&directory).await?;
            while let Some(entry) = entries.next_entry().await? {
                count += 1;
                ensure!(
                    count <= MAX_SCAN_ENTRIES,
                    "native source discovery exceeds entry limit"
                );
                let kind = entry.file_type().await?;
                if kind.is_symlink() {
                    continue;
                }
                let path = entry.path();
                if kind.is_dir() {
                    // Claude projects are one directory deep. Only descend
                    // into the requested session's child transcript tree.
                    if self.root.harness == Harness::Codex
                        || depth == 0
                        || entry.file_name() == native_id
                        || entry.file_name() == "subagents"
                    {
                        pending.push((path, depth + 1));
                    }
                    continue;
                }
                if !kind.is_file() {
                    continue;
                }
                let name = entry.file_name().to_string_lossy().into_owned();
                let agent_id = match self.root.harness {
                    Harness::Codex if name.ends_with(&format!("{native_id}.jsonl")) => None,
                    Harness::ClaudeCode if name == format!("{native_id}.jsonl") => None,
                    Harness::ClaudeCode
                        if path
                            .parent()
                            .and_then(Path::file_name)
                            .is_some_and(|name| name == "subagents")
                            && path
                                .parent()
                                .and_then(Path::parent)
                                .and_then(Path::file_name)
                                .is_some_and(|name| name == native_id) =>
                    {
                        let Some(agent) = name
                            .strip_prefix("agent-")
                            .and_then(|name| name.strip_suffix(".jsonl"))
                        else {
                            continue;
                        };
                        safe_native_component(agent)?;
                        Some(agent.to_owned())
                    }
                    _ => continue,
                };
                ensure!(
                    found.len() < MAX_GRAPH_ITEMS,
                    "too many native execution sources"
                );
                found.push((
                    NodeKey {
                        namespace: self.root.namespace.clone(),
                        harness: self.root.harness,
                        native_id: native_id.into(),
                        agent_id,
                    },
                    path,
                ));
            }
        }
        found.sort_by(|left, right| left.1.cmp(&right.1));
        Ok(found)
    }

    /// Ingest complete lines only. Replacement/truncation gets a new generation;
    /// an interrupted line is retried at the same offset on the next pass.
    /// A file identity locator survives moves within the registered root.
    pub async fn reconcile(
        &self,
        node: NodeKey,
        path: &Path,
        bound: Option<FileBound>,
    ) -> Result<CollectedSource> {
        node.validate()?;
        ensure!(
            node.harness == self.root.harness && node.namespace == self.root.namespace,
            "native root mismatch"
        );
        let path = tokio::fs::canonicalize(path).await?;
        let mut contained = false;
        for directory in self.directories() {
            if tokio::fs::try_exists(&directory).await? {
                contained |= path.starts_with(tokio::fs::canonicalize(directory).await?);
            }
        }
        ensure!(
            contained,
            "native transcript is outside its registered root"
        );
        let mut file = File::open(&path).await?;
        let metadata = file.metadata().await?;
        ensure!(
            metadata.is_file(),
            "native transcript must be a regular file"
        );
        let identity = file_identity(&metadata, &path)?;
        let format = match node.harness {
            Harness::Codex => SourceFormat::CodexRollout,
            Harness::ClaudeCode => SourceFormat::ClaudeTranscript,
        };
        let mut source = self
            .store
            .register(SourceDescriptor {
                namespace: node.namespace.clone(),
                harness: node.harness,
                format,
                locator: format!("file:{identity}"),
                node: Some(node.clone()),
            })
            .await?;
        let mut gaps = BTreeSet::new();
        let mut offset = source.cursor.offset;
        let mut hasher = Sha256::new();
        let mut generation = source.cursor.generation.clone();
        if let Some(cut) = bound.as_ref().and_then(|bound| bound.end_byte_offset)
            && cut <= source.cursor.offset
            && cut <= metadata.len()
            && source.cursor.next_sequence > 0
        {
            let prefix = self
                .finish_source(source.clone(), path.clone(), BTreeSet::new(), bound.clone())
                .await?;
            if prefix.gaps.is_empty()
                && let Some(range) = &prefix.range
                && self.verify_stored_prefix(&mut file, range, cut).await?
            {
                return Ok(prefix);
            }
        }
        if offset > metadata.len() || !generation.starts_with(&format!("{identity}/")) {
            offset = 0;
        } else if offset > 0 {
            ensure!(
                offset <= MAX_PREFIX_BYTES,
                "native transcript prefix exceeds reconciliation limit"
            );
            hash_prefix(&mut file, offset, &mut hasher).await?;
            if hash_digest(&hasher) != source.cursor.anchor_digest {
                offset = 0;
            }
        }
        if offset == 0 {
            if source.cursor.next_sequence > 0 {
                gaps.insert("source_replaced_or_truncated".into());
            }
            generation = if source.cursor.next_sequence > 0 {
                format!("{identity}/replacement/{}", uuid::Uuid::new_v4())
            } else {
                format!("{identity}/{}", uuid::Uuid::new_v4())
            };
            hasher = Sha256::new();
        }
        // A bounded fork import never advances beyond its declared byte cut.
        let max_offset = bound
            .as_ref()
            .and_then(|bound| bound.end_byte_offset)
            .unwrap_or(metadata.len())
            .min(metadata.len());
        if offset > max_offset {
            // An already ingested source may contain later parent execution.
            // Select the immutable prefix from stored ordinal evidence below.
            return self.finish_source(source, path, gaps, bound).await;
        }
        file.seek(SeekFrom::Start(offset)).await?;
        let mut reader = BufReader::new(file.take(max_offset - offset));
        let mut next_sequence = if generation == source.cursor.generation {
            source.cursor.next_sequence
        } else {
            0
        };
        let mut version = None;
        loop {
            let mut line = Vec::new();
            let bytes = (&mut reader)
                .take(MAX_RECORD_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
                .await?;
            if bytes == 0 {
                break;
            }
            if line.len() > MAX_RECORD_BYTES {
                gaps.insert("record_size_limit".into());
                break;
            }
            if line.last() != Some(&b'\n') {
                gaps.insert("partial_native_line".into());
                break;
            }
            let raw: Value = match serde_json::from_slice(&line) {
                Ok(value) => value,
                Err(_) => {
                    gaps.insert("invalid_native_json".into());
                    break;
                }
            };
            if let Err(error) = verify_identity(&node, &raw, next_sequence) {
                tracing::warn!(%error, "native transcript identity rejected");
                gaps.insert("native_identity_mismatch".into());
                break;
            }
            if let Some(producer) = raw
                .get("version")
                .and_then(Value::as_str)
                .or_else(|| raw.pointer("/payload/cli_version").and_then(Value::as_str))
            {
                version = Some(producer.to_owned());
            }
            if next_sequence >= MAX_RECORDS as u64 {
                gaps.insert("source_record_limit".into());
                break;
            }
            hasher.update(&line);
            offset += bytes as u64;
            let record = RecordInput {
                generation: generation.clone(),
                sequence: next_sequence,
                byte_start: Some(offset - bytes as u64),
                byte_end: Some(offset),
                producer_version: version.clone(),
                raw,
            };
            next_sequence += 1;
            source = self
                .store
                .append(
                    &source,
                    &[record],
                    SourceCursor {
                        generation: generation.clone(),
                        offset,
                        next_sequence,
                        anchor_digest: hash_digest(&hasher),
                    },
                )
                .await?;
        }
        let after = reader.into_inner().into_inner().metadata().await?;
        if file_identity(&after, &path)? != identity || after.len() < offset {
            gaps.insert("source_changed_during_read".into());
        }
        self.finish_source(source, path, gaps, bound).await
    }

    /// Validate only the requested immutable prefix. Changes after a fork's
    /// cut must neither enter its context nor invalidate intact prior evidence.
    async fn verify_stored_prefix(
        &self,
        file: &mut File,
        range: &SourceRange,
        cut: u64,
    ) -> Result<bool> {
        file.seek(SeekFrom::Start(0)).await?;
        let mut reader = BufReader::new(file.take(cut));
        let mut offset = 0;
        for sequence in range.start..range.end {
            let records = self
                .store
                .records(&SourceRange {
                    start: sequence,
                    end: sequence + 1,
                    ..range.clone()
                })
                .await?;
            let Some(record) = records.first() else {
                return Ok(false);
            };
            let mut line = vec![];
            let count = (&mut reader)
                .take(MAX_RECORD_BYTES as u64 + 1)
                .read_until(b'\n', &mut line)
                .await?;
            if count > MAX_RECORD_BYTES || line.last() != Some(&b'\n') {
                return Ok(false);
            }
            let raw: Value = match serde_json::from_slice(&line) {
                Ok(raw) => raw,
                Err(_) => return Ok(false),
            };
            if record.input.byte_start != Some(offset)
                || record.input.byte_end != Some(offset + count as u64)
                || record.input.raw != raw
            {
                return Ok(false);
            }
            offset += count as u64;
        }
        Ok(offset == cut)
    }

    async fn finish_source(
        &self,
        source: RegisteredSource,
        path: PathBuf,
        mut gaps: BTreeSet<String>,
        bound: Option<FileBound>,
    ) -> Result<CollectedSource> {
        if source.cursor.generation.contains("/replacement/") {
            gaps.insert("source_replaced_or_truncated".into());
        }
        let mut end = source.cursor.next_sequence;
        let mut metadata = None;
        if end > 0 {
            let first = self
                .store
                .records(&SourceRange {
                    source_id: source.id.clone(),
                    generation: source.cursor.generation.clone(),
                    start: 0,
                    end: 1,
                })
                .await?;
            metadata = first
                .first()
                .filter(|record| {
                    record.input.raw.get("type").and_then(Value::as_str) == Some("session_meta")
                })
                .and_then(|record| record.input.raw.get("payload"))
                .cloned();
        }
        if let Some(bound) = bound {
            ensure!(
                bound.end_ordinal_exclusive.is_some() || bound.end_byte_offset.is_some(),
                "empty file bound"
            );
            end = 0;
            let mut ordinal_next = metadata
                .as_ref()
                .and_then(|meta| {
                    meta.pointer("/history_base/end_ordinal_exclusive")
                        .and_then(Value::as_u64)
                })
                .unwrap_or(0);
            let mut byte_end = 0;
            while end < source.cursor.next_sequence {
                if bound
                    .end_ordinal_exclusive
                    .is_none_or(|cut| ordinal_next == cut)
                    && bound.end_byte_offset.is_none_or(|cut| byte_end == cut)
                {
                    break;
                }
                let records = self
                    .store
                    .records(&SourceRange {
                        source_id: source.id.clone(),
                        generation: source.cursor.generation.clone(),
                        start: end,
                        end: end + 1,
                    })
                    .await?;
                let Some(record) = records.first() else {
                    gaps.insert("parent_range_missing".into());
                    break;
                };
                if let Some(ordinal_end) = bound.end_ordinal_exclusive {
                    let Some(ordinal) = record.input.raw.get("ordinal").and_then(Value::as_u64)
                    else {
                        gaps.insert("parent_ordinal_unsupported".into());
                        break;
                    };
                    if ordinal != ordinal_next {
                        gaps.insert("parent_ordinal_gap".into());
                        break;
                    }
                    if ordinal >= ordinal_end {
                        break;
                    }
                }
                let (Some(start), Some(stop)) = (record.input.byte_start, record.input.byte_end)
                else {
                    gaps.insert("parent_byte_positions_missing".into());
                    break;
                };
                if start != byte_end {
                    gaps.insert("parent_byte_gap".into());
                    break;
                }
                if bound.end_byte_offset.is_some_and(|cut| stop > cut) {
                    break;
                }
                end += 1;
                byte_end = stop;
                ordinal_next += 1;
            }
            if bound
                .end_ordinal_exclusive
                .is_some_and(|cut| ordinal_next != cut)
            {
                gaps.insert("parent_ordinal_cut_missing".into());
            }
            if bound.end_byte_offset.is_some_and(|cut| byte_end != cut) {
                gaps.insert("parent_byte_cut_missing".into());
            }
        }
        // Filenames locate a candidate, but cannot alone prove attribution.
        if source.descriptor.harness == Harness::ClaudeCode && end > 0 {
            let mut observed = false;
            for sequence in 0..end {
                let records = self
                    .store
                    .records(&SourceRange {
                        source_id: source.id.clone(),
                        generation: source.cursor.generation.clone(),
                        start: sequence,
                        end: sequence + 1,
                    })
                    .await?;
                if let Some(record) = records.first()
                    && let Some(node) = &source.descriptor.node
                {
                    let raw = &record.input.raw;
                    if raw.get("sessionId").and_then(Value::as_str) == Some(node.native_id.as_str())
                        && (node.agent_id.is_none()
                            || raw.get("agentId").and_then(Value::as_str)
                                == node.agent_id.as_deref())
                    {
                        observed = true;
                        break;
                    }
                }
            }
            if !observed {
                gaps.insert("native_identity_unverified".into());
            }
        }
        let range = (end > 0).then(|| SourceRange {
            source_id: source.id.clone(),
            generation: source.cursor.generation.clone(),
            start: 0,
            end,
        });
        if range.is_none() {
            gaps.insert("native_history_unavailable".into());
        }
        Ok(CollectedSource {
            source,
            range,
            path: Some(path),
            gaps,
            metadata,
        })
    }
}

/// Native checkpoint references are exclusive. No wall-clock approximation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileBound {
    pub end_ordinal_exclusive: Option<u64>,
    pub end_byte_offset: Option<u64>,
}

pub fn codex_parent(metadata: &Value) -> Result<Option<(String, FileBound)>> {
    // Paginated rollouts reference a parent prefix instead of copying it.
    // These fields are internal/unstable; absent bounds cannot imply complete
    // fork provenance. Public lifecycle: https://learn.chatgpt.com/docs/app-server
    if let Some(base) = metadata
        .get("history_base")
        .filter(|value| !value.is_null())
    {
        let parent = base
            .get("thread_id")
            .and_then(Value::as_str)
            .context("missing history base thread")?;
        safe_native_component(parent)?;
        let ordinal = base
            .get("end_ordinal_exclusive")
            .and_then(Value::as_u64)
            .context("missing history base ordinal")?;
        let bytes = base
            .get("end_byte_offset")
            .and_then(Value::as_u64)
            .context("missing history base byte cut")?;
        return Ok(Some((
            parent.into(),
            FileBound {
                end_ordinal_exclusive: Some(ordinal),
                end_byte_offset: Some(bytes),
            },
        )));
    }
    Ok(None)
}

fn verify_identity(node: &NodeKey, raw: &Value, sequence: u64) -> Result<()> {
    ensure!(raw.is_object(), "native record is not an object");
    match node.harness {
        Harness::Codex => {
            if sequence == 0 {
                ensure!(
                    raw.get("type").and_then(Value::as_str) == Some("session_meta"),
                    "rollout metadata missing"
                );
            }
            if raw.get("type").and_then(Value::as_str) == Some("session_meta") {
                ensure!(
                    raw.pointer("/payload/id").and_then(Value::as_str)
                        == Some(node.native_id.as_str()),
                    "rollout thread mismatch"
                );
            }
        }
        Harness::ClaudeCode => {
            if let Some(session) = raw.get("sessionId").and_then(Value::as_str) {
                ensure!(session == node.native_id, "transcript session mismatch");
            }
            if let Some(agent) = raw.get("agentId").and_then(Value::as_str) {
                ensure!(
                    node.agent_id.as_deref() == Some(agent),
                    "transcript child mismatch"
                );
            }
        }
    }
    Ok(())
}

fn safe_native_component(value: &str) -> Result<()> {
    super::types::identifier(value)?;
    ensure!(
        value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_')),
        "invalid native path component"
    );
    Ok(())
}

async fn hash_prefix(file: &mut File, count: u64, hasher: &mut Sha256) -> Result<()> {
    file.seek(SeekFrom::Start(0)).await?;
    let mut remaining = count;
    let mut bytes = vec![0u8; 64 * 1024];
    while remaining > 0 {
        let length = bytes.len().min(remaining as usize);
        let read = file.read(&mut bytes[..length]).await?;
        ensure!(read > 0, "native file truncated during prefix verification");
        hasher.update(&bytes[..read]);
        remaining -= read as u64;
    }
    Ok(())
}

fn hash_digest(hasher: &Sha256) -> String {
    format!("sha256:{}", hex::encode(hasher.clone().finalize()))
}

fn file_identity(metadata: &std::fs::Metadata, path: &Path) -> Result<String> {
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        let _ = path;
        canonical_digest(&(metadata.dev(), metadata.ino(), metadata.created().ok()))
    }
    #[cfg(not(unix))]
    {
        canonical_digest(&(path, metadata.created().ok()))
    }
}

#[cfg(test)]
mod tests;
