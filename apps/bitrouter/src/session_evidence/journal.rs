//! Durable protocol observations and replay of application-owned native spools.

use std::collections::BTreeSet;
use std::path::Path;

use anyhow::{Context, Result, ensure};
use serde_json::Value;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncBufReadExt, AsyncReadExt, BufReader};
use tokio::sync::Mutex;

use super::store::EvidenceStore;
use super::types::{
    MAX_RECORD_BYTES, RecordInput, RegisteredSource, SourceCursor, SourceDescriptor, SourceFormat,
    SourceRange, StoredRecord,
};
use crate::eval::types::canonical_digest;

pub struct Journal {
    store: EvidenceStore,
    source: Mutex<RegisteredSource>,
    producer_version: String,
}

impl Journal {
    pub async fn new(
        store: EvidenceStore,
        descriptor: SourceDescriptor,
        producer_version: String,
    ) -> Result<Self> {
        let source = store.register(descriptor).await?;
        Ok(Self {
            store,
            source: Mutex::new(source),
            producer_version,
        })
    }

    /// One source per controller instance. Serializing the append supplies
    /// backpressure and keeps the durable ordering ahead of ACP delivery.
    pub async fn append(&self, raw: Value) -> Result<()> {
        self.append_record(raw).await?;
        Ok(())
    }

    /// Return the exact committed record so subprocess configuration can refer
    /// to its durable provenance before the adapter receives the request.
    pub async fn append_record(&self, raw: Value) -> Result<StoredRecord> {
        let mut source = self.source.lock().await;
        // A cancelled commit may have reached the database without updating
        // this cache. Re-read the durable cursor before the next observation.
        *source = self.store.register(source.descriptor.clone()).await?;
        ensure!(
            source.cursor.next_sequence < i64::MAX as u64,
            "controller journal sequence overflow"
        );
        let record = RecordInput {
            generation: "controller/1".into(),
            sequence: source.cursor.next_sequence,
            byte_start: None,
            byte_end: None,
            producer_version: Some(self.producer_version.clone()),
            raw,
        };
        let cursor = SourceCursor {
            generation: record.generation.clone(),
            offset: 0,
            next_sequence: record.sequence + 1,
            anchor_digest: canonical_digest(&(&source.cursor.anchor_digest, &record))?,
        };
        let committed = StoredRecord {
            id: record.id(&source.id)?,
            source_id: source.id.clone(),
            digest: canonical_digest(&record)?,
            input: record.clone(),
        };
        *source = if source.descriptor.format == SourceFormat::Acp {
            self.store
                .append_observation(&source, record, cursor)
                .await?
        } else {
            self.store.append(&source, &[record], cursor).await?
        };
        Ok(committed)
    }
}

pub struct SpoolImport {
    pub source: RegisteredSource,
    /// Complete committed records only; callers can replay this range safely.
    pub range: Option<SourceRange>,
    pub gaps: BTreeSet<String>,
}

/// Each proxy/hook invocation creates a unique file in a private controller
/// directory. Unlike a harness transcript, replacement is never legitimate:
/// leave a visible gap instead of adopting bytes under an old invocation id.
pub async fn import_spool(
    store: &EvidenceStore,
    directory: &Path,
    path: &Path,
    descriptor: SourceDescriptor,
) -> Result<SpoolImport> {
    let directory = tokio::fs::canonicalize(directory).await?;
    let metadata = tokio::fs::symlink_metadata(path).await?;
    ensure!(
        metadata.is_file() && !metadata.file_type().is_symlink(),
        "spool is not a regular file"
    );
    let path = tokio::fs::canonicalize(path).await?;
    ensure!(
        path.parent() == Some(directory.as_path()),
        "spool is outside controller directory"
    );
    let mut source = store.register(descriptor).await?;
    let start_sequence = source.cursor.next_sequence;
    let mut gaps = BTreeSet::new();
    let file = tokio::fs::File::open(path).await?;
    // Commit the known extent before any bounded-prefix import. If collection
    // is cancelled or the file disappears, its uncollected tail stays visible.
    let mut observed_end = file.metadata().await?.len();
    store.observe_spool_extent(&source, observed_end).await?;
    let mut reader = BufReader::new(file);
    let mut hasher = Sha256::new();
    let mut remaining = source.cursor.offset;
    ensure!(remaining <= 1024 * 1024 * 1024, "spool prefix size limit");
    let mut buffer = vec![0; 64 * 1024];
    while remaining > 0 {
        let length = buffer.len().min(remaining as usize);
        let read = reader.read(&mut buffer[..length]).await?;
        if read == 0 {
            break;
        }
        hasher.update(&buffer[..read]);
        remaining -= read as u64;
    }
    if remaining != 0
        || (source.cursor.next_sequence > 0 && digest(&hasher) != source.cursor.anchor_digest)
    {
        gaps.insert("native_spool_replaced".into());
        return Ok(SpoolImport {
            source,
            range: None,
            gaps,
        });
    }
    let mut offset = source.cursor.offset;
    let mut imported_records = 0;
    let mut version = None;
    if source.cursor.next_sequence > 0 {
        let records = store
            .records(&SourceRange {
                source_id: source.id.clone(),
                generation: source.cursor.generation.clone(),
                start: 0,
                end: 1,
            })
            .await?;
        version = records
            .first()
            .and_then(|record| record.input.producer_version.clone());
    }
    loop {
        let mut bytes = vec![];
        let count = (&mut reader)
            .take(MAX_RECORD_BYTES as u64 + 1)
            .read_until(b'\n', &mut bytes)
            .await?;
        if count == 0 {
            break;
        }
        let read_end = offset
            .checked_add(count as u64)
            .context("spool byte extent overflow")?;
        if read_end > observed_end {
            store.observe_spool_extent(&source, read_end).await?;
            observed_end = read_end;
        }
        if count > MAX_RECORD_BYTES {
            gaps.insert("native_spool_record_limit".into());
            break;
        }
        if bytes.last() != Some(&b'\n') {
            gaps.insert("native_spool_partial_line".into());
            break;
        }
        let raw: Value = match serde_json::from_slice(&bytes) {
            Ok(raw) => raw,
            Err(_) => {
                gaps.insert("native_spool_invalid_json".into());
                break;
            }
        };
        ensure!(raw.is_object(), "native spool event is not an object");
        if imported_records == 128 {
            gaps.insert("native_spool_backlog".into());
            break;
        }
        if let Some(value) = raw.get("version").and_then(Value::as_str) {
            version = Some(value.into());
        }
        hasher.update(&bytes);
        let record = RecordInput {
            generation: "spool/1".into(),
            sequence: source.cursor.next_sequence,
            byte_start: Some(offset),
            byte_end: Some(offset + count as u64),
            producer_version: version.clone(),
            raw,
        };
        offset += count as u64;
        let cursor = SourceCursor {
            generation: record.generation.clone(),
            offset,
            next_sequence: record.sequence + 1,
            anchor_digest: digest(&hasher),
        };
        source = store.append(&source, &[record], cursor).await?;
        imported_records += 1;
    }
    let range = (source.cursor.next_sequence > start_sequence).then(|| SourceRange {
        source_id: source.id.clone(),
        generation: source.cursor.generation.clone(),
        start: start_sequence,
        end: source.cursor.next_sequence,
    });
    Ok(SpoolImport {
        source,
        range,
        gaps,
    })
}

fn digest(hasher: &Sha256) -> String {
    format!("sha256:{}", hex::encode(hasher.clone().finalize()))
}

#[cfg(test)]
mod tests;
