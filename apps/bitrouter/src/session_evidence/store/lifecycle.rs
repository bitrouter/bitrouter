//! Durable lifecycle request/response boundaries, independent of Query state.
//! Each half is immutable so recovery can encounter profiles in either order.
//! <https://agentclientprotocol.com/protocol/v1/session-setup>

use super::*;
use crate::session_evidence::types::{Harness, RecordRef, SourceFormat};
use serde::{Deserialize, Serialize};
use serde_json::Value;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Boundary {
    id: String,
    revision: u64,
    controller_id: String,
    operation_id: String,
    harness: Harness,
    method: String,
    record: RecordRef,
}

pub(crate) struct OperationRecord {
    pub source: RegisteredSource,
    pub record: StoredRecord,
}

pub(crate) struct LifecycleOperation {
    pub request: Option<OperationRecord>,
    pub response: Option<OperationRecord>,
}

impl EvidenceStore {
    pub(super) async fn index_lifecycle_record(
        &self,
        db: &impl ConnectionTrait,
        source: &RegisteredSource,
        record: &RecordInput,
    ) -> Result<()> {
        if source.descriptor.format != SourceFormat::Acp {
            return Ok(());
        }
        let raw = &record.raw;
        let Some(method) = raw
            .get("method")
            .and_then(Value::as_str)
            .filter(|method| lifecycle(method))
        else {
            return Ok(());
        };
        let Some(phase @ ("request" | "response")) = raw.get("phase").and_then(Value::as_str)
        else {
            return Ok(());
        };
        ensure!(
            source.descriptor.node.is_none() && record.generation == "controller/1",
            "lifecycle boundary must belong to a controller journal"
        );
        let controller = source
            .descriptor
            .locator
            .strip_prefix("controller:")
            .context("lifecycle controller missing")?;
        let operation = raw
            .get("operation_id")
            .and_then(Value::as_str)
            .context("lifecycle operation missing")?;
        let key = operation_key(controller, operation)?;
        let boundary = Boundary {
            id: key.clone(),
            revision: 0,
            controller_id: controller.into(),
            operation_id: operation.into(),
            harness: source.descriptor.harness,
            method: method.into(),
            record: RecordRef::from_record(&StoredRecord {
                id: record.id(&source.id)?,
                source_id: source.id.clone(),
                digest: canonical_digest(record)?,
                input: record.clone(),
            })?,
        };
        let other = if phase == "request" {
            "response"
        } else {
            "request"
        };
        if let Some((existing, _)) = self
            .lifecycle_boundary(db, controller, operation, other)
            .await?
        {
            ensure!(
                existing.harness == boundary.harness && existing.method == boundary.method,
                "lifecycle response does not match its request"
            );
        }
        // append_observation already owns the writer transaction. A conflicting
        // boundary rolls back the raw record and cursor together, before ACP
        // forwarding. Backfill never rewrites the first recorded boundary.
        self.insert_object(db, &kind(phase), &key, 0, &boundary)
            .await
    }

    /// Rebuild only the requested owned range. Request and response may reside
    /// in different profile journals, and either can be recovered first.
    pub(crate) async fn index_lifecycle_range(
        &self,
        range: &SourceRange,
    ) -> Result<BTreeSet<String>> {
        let mut gaps = BTreeSet::new();
        let source = self
            .source(&range.source_id)
            .await?
            .context("lifecycle source missing")?;
        if source.descriptor.format != SourceFormat::Acp {
            return Ok(gaps);
        }
        let records = self.records(range).await?;
        ensure!(
            records.len() as u64 == range.end - range.start,
            "incomplete lifecycle range"
        );
        for record in records {
            // Every half is an immutable, idempotent object. Autocommit avoids
            // SQLite read-to-write transaction upgrades during recovery. Reads
            // verify both halves even when separate sources were indexed first.
            if let Err(error) = self
                .index_lifecycle_record(&self.db, &source, &record.input)
                .await
            {
                // A damaged derived boundary must not hide intact raw records
                // or prevent unrelated later operations from being recovered.
                tracing::warn!(%error, "historical lifecycle index is invalid");
                gaps.insert("native_lifecycle_index_invalid".into());
            }
        }
        Ok(gaps)
    }

    pub(crate) async fn lifecycle_operation(
        &self,
        controller: &str,
        operation: &str,
    ) -> Result<LifecycleOperation> {
        let request = self
            .lifecycle_boundary(&self.db, controller, operation, "request")
            .await?;
        let response = self
            .lifecycle_boundary(&self.db, controller, operation, "response")
            .await?;
        if let (Some((left, _)), Some((right, _))) = (&request, &response) {
            ensure!(
                left.method == right.method && left.harness == right.harness,
                "lifecycle boundary pair is inconsistent"
            );
            ensure!(
                left.record.range.source_id != right.record.range.source_id
                    || left.record.range.start < right.record.range.start,
                "lifecycle response precedes its request"
            );
        }
        Ok(LifecycleOperation {
            request: request.map(|(_, record)| record),
            response: response.map(|(_, record)| record),
        })
    }

    async fn lifecycle_boundary(
        &self,
        db: &impl ConnectionTrait,
        controller: &str,
        operation: &str,
        phase: &str,
    ) -> Result<Option<(Boundary, OperationRecord)>> {
        let key = operation_key(controller, operation)?;
        let Some(row) = self.object(db, &kind(phase), &key).await? else {
            return Ok(None);
        };
        ensure!(row.revision == 0, "lifecycle boundary revision changed");
        let boundary: Boundary = decode_object(row)?;
        boundary.record.validate()?;
        ensure!(
            boundary.id == key
                && boundary.revision == 0
                && boundary.controller_id == controller
                && boundary.operation_id == operation
                && lifecycle(&boundary.method)
                && boundary.record.range.generation == "controller/1",
            "invalid lifecycle boundary identity"
        );
        let row = source_entity::Entity::find_by_id(&boundary.record.range.source_id)
            .filter(source_entity::Column::Owner.eq(&self.owner_key))
            .one(db)
            .await?
            .context("lifecycle boundary source missing")?;
        ensure!(row.owner == self.owner_key, "foreign lifecycle source");
        let source = decode_source(row)?;
        ensure!(
            source.descriptor.format == SourceFormat::Acp
                && source.descriptor.node.is_none()
                && source.descriptor.harness == boundary.harness
                && source.descriptor.locator == format!("controller:{controller}"),
            "lifecycle boundary source mismatch"
        );
        let records = range_records(db, &self.owner_key, &boundary.record.range).await?;
        let record = records
            .into_iter()
            .next()
            .context("lifecycle boundary record missing")?;
        let raw = &record.input.raw;
        ensure!(
            RecordRef::from_record(&record)? == boundary.record
                && raw.get("operation_id").and_then(Value::as_str) == Some(operation)
                && raw.get("method").and_then(Value::as_str) == Some(boundary.method.as_str())
                && raw.get("phase").and_then(Value::as_str) == Some(phase),
            "lifecycle boundary does not match its original record"
        );
        Ok(Some((boundary, OperationRecord { source, record })))
    }
}

fn lifecycle(method: &str) -> bool {
    matches!(
        method,
        "session/new"
            | "session/load"
            | "session/resume"
            | "session/fork"
            | "session/close"
            | "session/delete"
    )
}

fn kind(phase: &str) -> String {
    format!("lifecycle_{phase}")
}

fn operation_key(controller: &str, operation: &str) -> Result<String> {
    identifier(controller)?;
    identifier(operation)?;
    canonical_digest(&(controller, operation))
}

#[cfg(test)]
mod tests;
