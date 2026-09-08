//! Owner-scoped native prefix discovery and immutable fork bindings.

use super::*;
use crate::session_evidence::types::{ForkBinding, MAX_RECORDS, NodeKey, SourceFormat};

pub struct StoredPrefix {
    pub range: SourceRange,
    pub record_digests: BTreeMap<String, String>,
    /// Compare native content and byte/ordinal positions across generations;
    /// observation ids themselves are not execution identity.
    pub fingerprint: String,
}

impl EvidenceStore {
    pub async fn node_sources(&self, node: &NodeKey) -> Result<Vec<RegisteredSource>> {
        node.validate()?;
        let mut after = None;
        let mut found = vec![];
        let mut scanned = 0;
        loop {
            let page = self.sources(after.as_deref(), 128).await?;
            scanned += page.len();
            ensure!(scanned <= MAX_RECORDS, "native source index scan limit");
            after = page.last().map(|source| source.id.clone());
            let done = page.len() < 128;
            found.extend(
                page.into_iter()
                    .filter(|source| source.descriptor.node.as_ref() == Some(node)),
            );
            ensure!(
                found.len() <= MAX_GRAPH_ITEMS,
                "native source candidate limit"
            );
            if done {
                return Ok(found);
            }
        }
    }

    pub async fn generation_starts(
        &self,
        source_id: &str,
        after: Option<&str>,
    ) -> Result<Vec<StoredRecord>> {
        ensure!(
            self.source(source_id).await?.is_some(),
            "native source is not owned"
        );
        let mut query = record_entity::Entity::find()
            .filter(record_entity::Column::Owner.eq(&self.owner_key))
            .filter(record_entity::Column::SourceId.eq(source_id))
            .filter(record_entity::Column::SourceSequence.eq(0))
            .order_by_asc(record_entity::Column::Id)
            .limit(RECORD_PAGE_SIZE);
        if let Some(id) = after {
            digest_identifier(id)?;
            query = query.filter(record_entity::Column::Id.gt(id));
        }
        query
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| {
                ensure!(
                    row.owner == self.owner_key && row.source_id == source_id,
                    "foreign generation start"
                );
                decode_record(row)
            })
            .collect()
    }

    /// Validate native ordinal continuity and exact byte closure, including
    /// generations which are no longer the source's active cursor.
    pub async fn fork_prefix(
        &self,
        source: &RegisteredSource,
        first: &StoredRecord,
        ordinal: u64,
        bytes: u64,
    ) -> Result<Option<StoredPrefix>> {
        ensure!(
            source.id == source.descriptor.id(&self.owner_key)? && first.source_id == source.id,
            "foreign fork prefix"
        );
        ensure!(
            source.descriptor.format == SourceFormat::CodexRollout,
            "fork prefix is not a Codex rollout"
        );
        let Some(start_ordinal) = first
            .input
            .raw
            .get("ordinal")
            .and_then(serde_json::Value::as_u64)
        else {
            return Ok(None);
        };
        let Some(count) = ordinal
            .checked_sub(start_ordinal)
            .filter(|count| *count > 0 && *count <= MAX_RECORDS as u64)
        else {
            return Ok(None);
        };
        let node = source
            .descriptor
            .node
            .as_ref()
            .context("fork source node missing")?;
        let metadata = first
            .input
            .raw
            .get("payload")
            .context("fork metadata missing")?;
        if first.input.sequence != 0
            || first
                .input
                .raw
                .get("type")
                .and_then(serde_json::Value::as_str)
                != Some("session_meta")
            || metadata.get("id").and_then(serde_json::Value::as_str)
                != Some(node.native_id.as_str())
            || start_ordinal
                != metadata
                    .pointer("/history_base/end_ordinal_exclusive")
                    .and_then(serde_json::Value::as_u64)
                    .unwrap_or(0)
        {
            return Ok(None);
        }
        let range = SourceRange {
            source_id: source.id.clone(),
            generation: first.input.generation.clone(),
            start: 0,
            end: count,
        };
        let mut record_digests = BTreeMap::new();
        let mut fingerprint = canonical_digest(&"native-prefix/1")?;
        let mut start = 0;
        let mut byte_end = 0;
        while start < count {
            let end = (start + RECORD_PAGE_SIZE).min(count);
            let records = self
                .records(&SourceRange {
                    start,
                    end,
                    ..range.clone()
                })
                .await?;
            if records.len() as u64 != end - start {
                return Ok(None);
            }
            for record in records {
                ensure!(
                    record.input.sequence != 0 || &record == first,
                    "fork metadata differs from its stored record"
                );
                if record
                    .input
                    .raw
                    .get("ordinal")
                    .and_then(serde_json::Value::as_u64)
                    != Some(start_ordinal + record.input.sequence)
                    || record.input.byte_start != Some(byte_end)
                {
                    return Ok(None);
                }
                let Some(stop) = record.input.byte_end.filter(|stop| *stop <= bytes) else {
                    return Ok(None);
                };
                byte_end = stop;
                fingerprint = canonical_digest(&(
                    fingerprint,
                    record.input.byte_start,
                    record.input.byte_end,
                    &record.input.raw,
                ))?;
                record_digests.insert(record.id, record.digest);
            }
            start = end;
        }
        if byte_end != bytes {
            return Ok(None);
        }
        Ok(Some(StoredPrefix {
            range,
            record_digests,
            fingerprint,
        }))
    }

    pub async fn fork_binding(&self, id: &str) -> Result<Option<ForkBinding>> {
        let binding: Option<ForkBinding> = self
            .object(&self.db, "fork_binding", id)
            .await?
            .map(decode_object)
            .transpose()?;
        if let Some(binding) = &binding {
            binding.validate()?;
        }
        Ok(binding)
    }

    pub async fn bind_fork(&self, binding: &ForkBinding) -> Result<()> {
        self.verify_fork_binding(binding).await?;
        self.insert_object(&self.db, "fork_binding", &binding.id, 0, binding)
            .await
    }

    pub(crate) async fn verify_fork_binding(&self, binding: &ForkBinding) -> Result<()> {
        binding.validate()?;
        let child = record_entity::Entity::find_by_id(&binding.child_record_id)
            .filter(record_entity::Column::Owner.eq(&self.owner_key))
            .one(&self.db)
            .await?
            .context("fork child record is not owned")?;
        ensure!(child.owner == self.owner_key, "foreign fork child record");
        let child = decode_record(child)?;
        ensure!(
            child.digest == binding.child_record_digest,
            "fork child digest mismatch"
        );
        let source = self
            .source(&child.source_id)
            .await?
            .context("fork child source missing")?;
        ensure!(
            source.descriptor.node.as_ref() == Some(&binding.child)
                && source.descriptor.format == SourceFormat::CodexRollout,
            "fork child source mismatch"
        );
        let child_rollout = binding
            .rollouts
            .as_ref()
            .map_or(binding.child.native_id.as_str(), |ids| &ids.child);
        let parent_rollout = binding
            .rollouts
            .as_ref()
            .map_or(binding.parent.native_id.as_str(), |ids| &ids.parent);
        let child_identity = self.rollout_identity(&source.id).await?;
        ensure!(
            child_identity
                .as_ref()
                .is_none_or(|identity| identity.rollout_id == child_rollout)
                && (binding.rollouts.is_none() || child_identity.is_some()),
            "bound child rollout identity mismatch"
        );
        // Bind only the native history-base fields, never a timestamp or a
        // similarly worded task. Public lifecycle: https://learn.chatgpt.com/docs/app-server
        let base = child
            .input
            .raw
            .pointer("/payload/history_base")
            .context("native fork reference missing")?;
        ensure!(
            child.input.sequence == 0
                && child
                    .input
                    .raw
                    .get("type")
                    .and_then(serde_json::Value::as_str)
                    == Some("session_meta")
                && child
                    .input
                    .raw
                    .pointer("/payload/id")
                    .and_then(serde_json::Value::as_str)
                    == Some(binding.child.native_id.as_str())
                && base.get("thread_id").and_then(serde_json::Value::as_str)
                    == Some(parent_rollout)
                && base
                    .get("end_ordinal_exclusive")
                    .and_then(serde_json::Value::as_u64)
                    == Some(binding.end_ordinal_exclusive)
                && base
                    .get("end_byte_offset")
                    .and_then(serde_json::Value::as_u64)
                    == Some(binding.end_byte_offset),
            "fork binding is not supported by native evidence"
        );
        let source = self
            .source(&binding.checkpoint.source_id)
            .await?
            .context("fork parent source missing")?;
        ensure!(
            source.descriptor.node.as_ref() == Some(&binding.parent),
            "fork parent source mismatch"
        );
        let parent_identity = self.rollout_identity(&source.id).await?;
        ensure!(
            parent_identity
                .as_ref()
                .is_none_or(|identity| identity.rollout_id == parent_rollout)
                && (binding.rollouts.is_none() || parent_identity.is_some()),
            "bound parent rollout identity mismatch"
        );
        let first = self
            .records(&SourceRange {
                end: 1,
                ..binding.checkpoint.clone()
            })
            .await?;
        let first = first.first().context("fork parent metadata missing")?;
        let prefix = self
            .fork_prefix(
                &source,
                first,
                binding.end_ordinal_exclusive,
                binding.end_byte_offset,
            )
            .await?
            .context("fork prefix does not close at native cut")?;
        ensure!(
            prefix.range == binding.checkpoint && prefix.record_digests == binding.record_digests,
            "fork prefix digest mismatch"
        );
        Ok(())
    }
}
