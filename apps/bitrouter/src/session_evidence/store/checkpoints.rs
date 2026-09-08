//! Freeze committed source positions without deriving task membership from
//! timestamps, arrival adjacency, or a mutable collection snapshot.

use serde::{Deserialize, Serialize};

use super::*;
use crate::session_evidence::checkpoint::{NativeCheckpoint, SourceFrontier};
use crate::session_evidence::types::{AcpSessionKey, PARSER_VERSION, RecordRef};

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct StoredCheckpoint {
    id: String,
    revision: u64,
    checkpoint: NativeCheckpoint,
}

impl EvidenceStore {
    pub(crate) async fn capture_checkpoint(
        &self,
        controller_id: &str,
        operation_id: &str,
        phase: &str,
        session: AcpSessionKey,
        gaps: BTreeSet<String>,
    ) -> Result<String> {
        let mut checkpoint = NativeCheckpoint {
            schema_version: 1,
            parser_version: PARSER_VERSION.into(),
            controller_id: controller_id.into(),
            operation_id: operation_id.into(),
            phase: phase.into(),
            session,
            sources: Vec::new(),
            gaps,
        };
        checkpoint.validate()?;
        let transaction = self.read_snapshot().await?;
        let mut after = None;
        let mut inspected = 0;
        'inventory: loop {
            let mut query = source_entity::Entity::find()
                .filter(source_entity::Column::Owner.eq(&self.owner_key))
                .order_by_asc(source_entity::Column::Id)
                .limit(16);
            if let Some(id) = &after {
                query = query.filter(source_entity::Column::Id.gt(id));
            }
            let rows = query.all(&transaction).await?;
            if rows.is_empty() {
                break;
            }
            for row in rows {
                if inspected == MAX_GRAPH_ITEMS {
                    checkpoint
                        .gaps
                        .insert("native_checkpoint_inventory_limit".into());
                    break 'inventory;
                }
                inspected += 1;
                after = Some(row.id.clone());
                let source = match decode_source(row) {
                    Ok(source) => source,
                    Err(error) => {
                        tracing::warn!(%error, "checkpoint source could not be read");
                        checkpoint
                            .gaps
                            .insert("native_checkpoint_source_invalid".into());
                        continue;
                    }
                };
                if source.descriptor.namespace != checkpoint.session.namespace
                    || source.descriptor.harness != checkpoint.session.harness
                {
                    continue;
                }
                match self.source_frontier(&transaction, source).await {
                    Ok(frontier) => {
                        if frontier.source.cursor == SourceCursor::default() {
                            checkpoint
                                .gaps
                                .insert("native_checkpoint_source_uninitialized".into());
                        }
                        checkpoint.sources.push(frontier);
                    }
                    Err(error) => {
                        tracing::warn!(%error, "checkpoint frontier could not be verified");
                        checkpoint
                            .gaps
                            .insert("native_checkpoint_frontier_invalid".into());
                    }
                }
            }
        }
        transaction.commit().await?;
        checkpoint.validate()?;
        let stored = StoredCheckpoint {
            id: checkpoint.digest()?,
            revision: 0,
            checkpoint,
        };
        self.insert_object(&self.db, "native_checkpoint", &stored.id, 0, &stored)
            .await?;
        Ok(stored.id)
    }

    async fn source_frontier(
        &self,
        db: &impl ConnectionTrait,
        source: RegisteredSource,
    ) -> Result<SourceFrontier> {
        let last_record = if source.cursor.next_sequence > 0 {
            let range = SourceRange {
                source_id: source.id.clone(),
                generation: source.cursor.generation.clone(),
                start: source.cursor.next_sequence - 1,
                end: source.cursor.next_sequence,
            };
            let records = range_records(db, &self.owner_key, &range).await?;
            ensure!(records.len() == 1, "frontier record missing or ambiguous");
            Some(RecordRef::from_record(
                records.first().context("frontier record missing")?,
            )?)
        } else {
            None
        };
        let frontier = SourceFrontier {
            source,
            last_record,
        };
        frontier.validate()?;
        Ok(frontier)
    }

    pub(super) async fn checkpoint_on(
        &self,
        db: &impl ConnectionTrait,
        id: &str,
    ) -> Result<NativeCheckpoint> {
        digest_identifier(id)?;
        let stored: StoredCheckpoint = decode_object(
            self.object(db, "native_checkpoint", id)
                .await?
                .context("native checkpoint missing")?,
        )?;
        ensure!(
            stored.revision == 0 && stored.id == stored.checkpoint.digest()?,
            "native checkpoint identity mismatch"
        );
        stored.checkpoint.validate()?;
        for frontier in &stored.checkpoint.sources {
            let source = source_entity::Entity::find_by_id(&frontier.source.id)
                .filter(source_entity::Column::Owner.eq(&self.owner_key))
                .one(db)
                .await?
                .context("checkpoint source missing")?;
            let source = decode_source(source)?;
            ensure!(
                source.descriptor == frontier.source.descriptor
                    && source.revision >= frontier.source.revision,
                "checkpoint source provenance changed"
            );
            if let Some(last) = &frontier.last_record {
                let records = range_records(db, &self.owner_key, &last.range).await?;
                ensure!(
                    records.len() == 1 && RecordRef::from_record(&records[0])? == *last,
                    "checkpoint frontier record changed"
                );
            }
        }
        Ok(stored.checkpoint)
    }
}

#[cfg(test)]
mod tests;
