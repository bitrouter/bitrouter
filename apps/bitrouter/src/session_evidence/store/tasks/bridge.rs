//! Producer observations are indexed by their original durable prompt, never
//! by the active session cursor or the next native event.

use super::*;
use crate::session_evidence::adapter_bridge::{
    self, Event, Observation, PromptEvidence, PromptOrigin, ProvenObservation,
};
use crate::session_evidence::types::RecordRef;

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeIndex {
    id: String,
    revision: u64,
    records: Vec<RecordRef>,
    gaps: BTreeSet<String>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct BridgeGap {
    id: String,
    revision: u64,
    record: RecordRef,
}

#[cfg(test)]
mod tests;

impl EvidenceStore {
    pub(super) async fn record_bridge_index_gap(
        &self,
        db: &impl ConnectionTrait,
        source: &RegisteredSource,
        input: &RecordInput,
    ) -> Result<()> {
        let observation: Observation = serde_json::from_value(
            input
                .raw
                .get("payload")
                .context("bridge gap payload missing")?
                .clone(),
        )?;
        let id = PromptOperation::key(
            &observation.origin.controller_id,
            &observation.origin.operation_id,
        )?;
        if self.object(db, "prompt_bridge_gap", &id).await?.is_some() {
            return Ok(());
        }
        let record = StoredRecord {
            id: input.id(&source.id)?,
            source_id: source.id.clone(),
            digest: canonical_digest(input)?,
            input: input.clone(),
        };
        self.insert_object(
            db,
            "prompt_bridge_gap",
            &id,
            0,
            &BridgeGap {
                id: id.clone(),
                revision: 0,
                record: RecordRef::from_record(&record)?,
            },
        )
        .await
    }

    async fn prompt_origin_on(
        &self,
        db: &impl ConnectionTrait,
        controller: &str,
        operation: &str,
    ) -> Result<Option<PromptOrigin>> {
        let Some(operation) = self
            .prompt_operation(db, &PromptOperation::key(controller, operation)?)
            .await?
        else {
            return Ok(None);
        };
        Ok(Some(PromptOrigin {
            controller_id: operation.controller_id,
            operation_id: operation.operation_id,
            session: operation.session,
            request: RecordRef {
                range: operation.request.range,
                record_id: operation.request.record_id,
                record_digest: operation.request.record_digest,
            },
        }))
    }

    pub(crate) async fn prompt_origin(
        &self,
        controller: &str,
        operation: &str,
    ) -> Result<Option<PromptOrigin>> {
        let transaction = self.read_snapshot().await?;
        let origin = self
            .prompt_origin_on(&transaction, controller, operation)
            .await?;
        transaction.commit().await?;
        Ok(origin)
    }

    async fn verify_bridge_observation(
        &self,
        db: &impl ConnectionTrait,
        source: &RegisteredSource,
        record: &StoredRecord,
    ) -> Result<Observation> {
        ensure!(
            source.descriptor.format == SourceFormat::Acp
                && source.descriptor.node.is_none()
                && record.source_id == source.id
                && record.input.raw.get("method").and_then(Value::as_str)
                    == Some(adapter_bridge::METHOD)
                && record.input.raw.get("phase").and_then(Value::as_str) == Some("notification"),
            "invalid adapter observation source"
        );
        RecordRef::from_record(record)?;
        let observation: Observation = serde_json::from_value(
            record
                .input
                .raw
                .get("payload")
                .context("adapter observation payload missing")?
                .clone(),
        )?;
        observation.validate()?;
        ensure!(
            source.descriptor.locator == format!("controller:{}", observation.origin.controller_id)
                && source.descriptor.harness == observation.origin.session.harness
                && record.input.producer_version.as_deref()
                    == Some(
                        format!(
                            "{}@{}",
                            observation.adapter.package, observation.adapter.version
                        )
                        .as_str()
                    ),
            "adapter observation producer mismatch"
        );
        let original = self
            .prompt_origin_on(
                db,
                &observation.origin.controller_id,
                &observation.origin.operation_id,
            )
            .await?
            .context("adapter observation original prompt missing")?;
        ensure!(
            original == observation.origin,
            "adapter observation original prompt mismatch"
        );
        Ok(observation)
    }

    pub(super) async fn index_bridge_record(
        &self,
        db: &impl ConnectionTrait,
        source: &RegisteredSource,
        input: &RecordInput,
    ) -> Result<()> {
        if input.raw.get("method").and_then(Value::as_str) != Some(adapter_bridge::METHOD) {
            return Ok(());
        }
        let record = StoredRecord {
            id: input.id(&source.id)?,
            source_id: source.id.clone(),
            digest: canonical_digest(input)?,
            input: input.clone(),
        };
        let observation = match self.verify_bridge_observation(db, source, &record).await {
            Ok(observation) => observation,
            Err(error) => {
                // Keep the raw notification. An invalid or unbound event
                // cannot create membership, hide missing coverage, or prevent
                // later healthy events from being collected.
                tracing::warn!(%error, "native adapter observation remains unbound");
                return Ok(());
            }
        };
        let id = PromptOperation::key(
            &observation.origin.controller_id,
            &observation.origin.operation_id,
        )?;
        let reference = RecordRef::from_record(&record)?;
        let old = self.object(db, "prompt_bridge", &id).await?;
        let Some(old) = old else {
            return self
                .insert_object(
                    db,
                    "prompt_bridge",
                    &id,
                    0,
                    &BridgeIndex {
                        id: id.clone(),
                        revision: 0,
                        records: vec![reference],
                        gaps: BTreeSet::new(),
                    },
                )
                .await;
        };
        let mut index: BridgeIndex = decode_object(old)?;
        ensure!(
            index.id == id && index.records.len() <= adapter_bridge::MAX_OBSERVATIONS as usize,
            "invalid adapter observation index"
        );
        if index.records.contains(&reference) {
            return Ok(());
        }
        if index.records.len() == adapter_bridge::MAX_OBSERVATIONS as usize {
            if !index.gaps.insert("native_bridge_observation_limit".into()) {
                return Ok(());
            }
        } else {
            index.records.push(reference);
        }
        let previous = index.revision;
        index.revision = previous
            .checked_add(1)
            .context("adapter observation revision overflow")?;
        self.replace_task_object(db, "prompt_bridge", &id, previous, &index)
            .await
    }

    pub(crate) async fn prompt_bridge_evidence(
        &self,
        session: &AcpSessionKey,
        expected_attempt: &str,
    ) -> Result<PromptEvidence> {
        let transaction = self.read_snapshot().await?;
        let mut evidence = PromptEvidence::default();
        if let Some(task) = self.active_task(&transaction, session).await? {
            ensure!(
                task.attempt_id == expected_attempt,
                "adapter evidence attempt changed; retry snapshot"
            );
            self.task_attempt(&transaction, &task).await?;
            'operations: for id in &task.operations {
                if let Some(row) = self.object(&transaction, "prompt_bridge_gap", id).await? {
                    let gap: BridgeGap = decode_object(row)?;
                    ensure!(gap.id == *id, "adapter index gap belongs to another prompt");
                    gap.record.validate()?;
                    evidence.gaps.insert("native_bridge_index_failed".into());
                }
                let operation = self
                    .prompt_operation(&transaction, id)
                    .await?
                    .context("adapter prompt missing")?;
                let Some(row) = self.object(&transaction, "prompt_bridge", id).await? else {
                    evidence.gaps.insert("native_bridge_unobserved".into());
                    continue;
                };
                let index: BridgeIndex = decode_object(row)?;
                ensure!(
                    index.id == *id
                        && index.records.len() <= adapter_bridge::MAX_OBSERVATIONS as usize,
                    "invalid adapter observation index"
                );
                evidence.gaps.extend(index.gaps);
                let mut sequences = BTreeSet::new();
                let mut finished = false;
                for reference in index.records {
                    if evidence.observations.len() == MAX_GRAPH_ITEMS {
                        evidence.gaps.insert("native_bridge_evidence_limit".into());
                        break 'operations;
                    }
                    reference.validate()?;
                    let rows =
                        range_records(&transaction, &self.owner_key, &reference.range).await?;
                    let record = rows.first().context("adapter observation record missing")?;
                    ensure!(
                        rows.len() == 1 && RecordRef::from_record(record)? == reference,
                        "adapter observation record mismatch"
                    );
                    let row = source_entity::Entity::find_by_id(&record.source_id)
                        .filter(source_entity::Column::Owner.eq(&self.owner_key))
                        .one(&transaction)
                        .await?
                        .context("adapter observation source missing")?;
                    ensure!(
                        row.owner == self.owner_key,
                        "foreign adapter observation source"
                    );
                    let observation = self
                        .verify_bridge_observation(&transaction, &decode_source(row)?, record)
                        .await?;
                    ensure!(
                        observation.origin.controller_id == operation.controller_id
                            && observation.origin.operation_id == operation.operation_id
                            && observation.origin.session == *session,
                        "adapter observation belongs to another prompt"
                    );
                    if !sequences.insert(observation.sequence) {
                        evidence
                            .gaps
                            .insert("native_bridge_sequence_conflict".into());
                    }
                    match &observation.event {
                        Event::Finished {
                            notification_failures,
                            ..
                        } => {
                            if finished {
                                evidence
                                    .gaps
                                    .insert("native_bridge_outcome_conflict".into());
                            }
                            finished = true;
                            if *notification_failures > 0 {
                                evidence.gaps.insert("native_bridge_delivery_failed".into());
                            }
                        }
                        Event::CodexCommandObserved { .. } => {
                            evidence
                                .gaps
                                .insert("native_bridge_command_causality_unverified".into());
                        }
                        _ => {}
                    }
                    evidence.observations.push(ProvenObservation {
                        record: reference,
                        observation,
                    });
                }
                if !finished {
                    evidence
                        .gaps
                        .insert("native_bridge_prompt_outcome_unobserved".into());
                }
                if sequences.is_empty() || sequences.iter().copied().ne(0..sequences.len() as u32) {
                    evidence.gaps.insert("native_bridge_sequence_gap".into());
                }
            }
        }
        transaction.commit().await?;
        Ok(evidence)
    }
}
