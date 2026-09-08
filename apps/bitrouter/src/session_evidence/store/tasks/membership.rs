//! Immutable, source-cut membership observations and an atomic attempt pointer.

use sha2::{Digest, Sha256};

use super::*;
use crate::session_evidence::membership::{AttemptExecutions, InspectedPrefix};
use crate::session_evidence::native_inputs::Scanner;
use crate::session_evidence::types::{MAX_RECORDS, RecordRef};

mod provenance;
mod uniqueness;

#[derive(Debug, Clone)]
pub(crate) struct PointerStamp {
    revision: i64,
    digest: String,
}

#[derive(Serialize, Deserialize)]
struct ExecutionPointer {
    id: String,
    revision: u64,
    session: AcpSessionKey,
    snapshot: String,
    attempt_revision: u64,
}

impl EvidenceStore {
    pub(crate) async fn execution_pointer_stamp(
        &self,
        attempt: &str,
    ) -> Result<Option<PointerStamp>> {
        digest_identifier(attempt)?;
        Ok(self
            .object(&self.db, "attempt_execution_pointer", attempt)
            .await?
            .map(|row| PointerStamp {
                revision: row.revision,
                digest: row.digest,
            }))
    }

    pub(crate) async fn record_attempt_executions(
        &self,
        expected: &Attempt,
        expected_pointer: Option<&PointerStamp>,
        mut evidence: AttemptExecutions,
    ) -> Result<(Attempt, AttemptExecutions)> {
        evidence.validate()?;
        ensure!(
            evidence.attempt_id == expected.id
                && evidence.attempt_revision == expected.revision
                && evidence.session == expected.session
                && evidence.prefixes.is_empty(),
            "execution membership attempt mismatch"
        );
        // Long original-evidence reads finish before the short independent
        // index write. No task/attempt row is updated or locked by this path.
        // https://www.sqlite.org/isolation.html
        let read = self.read_snapshot().await?;
        let task = self
            .task_for_attempt(&read, &expected.session, &expected.id)
            .await?
            .context("execution task missing")?;
        let mut attempt = self.task_attempt(&read, &task).await?;
        ensure!(
            attempt.id == expected.id
                && attempt.revision == expected.revision
                && attempt.phase != AttemptPhase::Ready,
            "execution task revision changed"
        );
        for range in membership_ranges(&evidence)? {
            evidence.prefixes.push(InspectedPrefix {
                digest: prefix_digest(&read, &self.owner_key, &range).await?,
                range,
            });
        }
        self.verify_execution_snapshot_on(&read, &evidence).await?;
        let mut id = canonical_digest(&evidence)?;
        // Preserve corrupt derived objects for diagnosis. A verified rebuild
        // gets another content address; it cannot poison the original task.
        for _ in 0..4 {
            let Some(row) = self.object(&read, "attempt_executions", &id).await? else {
                break;
            };
            if decode_object::<AttemptExecutions>(row).is_ok() {
                break;
            }
            evidence.recovered_from = Some(id);
            id = canonical_digest(&evidence)?;
        }
        read.commit().await?;
        let pointer = ExecutionPointer {
            id: attempt.id.clone(),
            revision: expected_pointer
                .map(|stamp| {
                    stamp
                        .revision
                        .max(-1)
                        .checked_add(1)
                        .context("execution pointer revision overflow")
                })
                .transpose()?
                .unwrap_or(0)
                .try_into()?,
            session: attempt.session.clone(),
            snapshot: id.clone(),
            attempt_revision: attempt.revision,
        };
        let transaction = self.db.begin().await?;
        if let Some(stamp) = expected_pointer {
            let changed = object_entity::Entity::update_many()
                .col_expr(
                    object_entity::Column::Revision,
                    Expr::value(i64::try_from(pointer.revision)?),
                )
                .col_expr(
                    object_entity::Column::ObjectJson,
                    Expr::value(serde_json::to_string(&pointer)?),
                )
                .col_expr(
                    object_entity::Column::Digest,
                    Expr::value(canonical_digest(&pointer)?),
                )
                .filter(
                    object_entity::Column::Id
                        .eq(self.object_id("attempt_execution_pointer", &attempt.id)?),
                )
                .filter(object_entity::Column::Owner.eq(&self.owner_key))
                .filter(object_entity::Column::Kind.eq("attempt_execution_pointer"))
                .filter(object_entity::Column::ObjectKey.eq(&attempt.id))
                .filter(object_entity::Column::Revision.eq(stamp.revision))
                .filter(object_entity::Column::Digest.eq(&stamp.digest))
                .exec(&transaction)
                .await?;
            ensure!(
                changed.rows_affected == 1,
                "execution pointer changed; retry inspection"
            );
        } else {
            self.insert_object(
                &transaction,
                "attempt_execution_pointer",
                &attempt.id,
                0,
                &pointer,
            )
            .await?;
        }
        // The first database access above is a write to the derived index.
        // Concurrent raw observations retain their existing lock order.
        let current: Attempt = decode_task_object(
            self.object(&transaction, "attempt", &attempt.id)
                .await?
                .context("execution attempt disappeared")?,
        )?;
        ensure!(
            current.revision == attempt.revision && current.session == attempt.session,
            "execution attempt advanced; retry inspection"
        );
        self.insert_object(&transaction, "attempt_executions", &id, 0, &evidence)
            .await?;
        transaction.commit().await?;
        attempt.members = evidence.members();
        attempt.execution_snapshot = Some(id);
        Ok((attempt, evidence))
    }

    /// Revalidate the original source cuts. This is an observation at those
    /// cuts, never a certificate that no later executions or conflicts exist.
    pub async fn attempt_executions(&self, id: &str) -> Result<AttemptExecutions> {
        let transaction = self.read_snapshot().await?;
        let evidence: AttemptExecutions = decode_object(
            self.object(&transaction, "attempt_executions", id)
                .await?
                .context("execution snapshot missing")?,
        )?;
        self.verify_execution_snapshot_on(&transaction, &evidence)
            .await?;
        transaction.commit().await?;
        Ok(evidence)
    }

    async fn verify_execution_snapshot_on(
        &self,
        db: &impl ConnectionTrait,
        evidence: &AttemptExecutions,
    ) -> Result<()> {
        evidence.validate()?;
        let expected = membership_ranges(evidence)?;
        ensure!(
            evidence.prefixes.len() == expected.len()
                && evidence
                    .prefixes
                    .iter()
                    .zip(&expected)
                    .all(|(prefix, range)| prefix.range == *range),
            "execution inspection ranges mismatch"
        );
        let mut references = evidence.references()?;
        let mut detail_budget = MAX_OBJECT_BYTES;
        let mut uniqueness = uniqueness::UniqueInputs::new(self, db, evidence).await?;
        for prefix in &evidence.prefixes {
            ensure!(
                prefix.digest == prefix_digest(db, &self.owner_key, &prefix.range).await?,
                "execution inspection prefix changed"
            );
            let row = source_entity::Entity::find_by_id(&prefix.range.source_id)
                .filter(source_entity::Column::Owner.eq(&self.owner_key))
                .one(db)
                .await?
                .context("execution source missing")?;
            let source = decode_source(row)?;
            let targets = uniqueness.targets.clone();
            let mut scanner = matches!(
                source.descriptor.format,
                SourceFormat::CodexAppServer | SourceFormat::ClaudeCli
            )
            .then(|| Scanner::new(&source.descriptor, &targets));
            let mut rollout = if source.descriptor.format == SourceFormat::CodexRollout
                && prefix.range.start == 0
            {
                Some(
                    crate::session_evidence::execution::rollout_runs::Scanner::new(
                        source
                            .descriptor
                            .node
                            .clone()
                            .context("execution rollout owner missing")?,
                    ),
                )
            } else {
                None
            };
            let mut start = prefix.range.start;
            while start < prefix.range.end {
                let range = SourceRange {
                    start,
                    end: (start + RECORD_PAGE_SIZE).min(prefix.range.end),
                    ..prefix.range.clone()
                };
                uniqueness
                    .claims(self, db, &source.descriptor, &range, evidence)
                    .await?;
                for record in range_records(db, &self.owner_key, &range).await? {
                    if let Some(reference) = references.remove(&record.id) {
                        ensure!(
                            RecordRef::from_record(&record)? == reference,
                            "execution original record changed"
                        );
                    }
                    if let Some(scanner) = &mut scanner {
                        scanner.push(&record)?;
                    }
                    if let Some(scanner) = &mut rollout {
                        scanner.push(&record, &mut detail_budget);
                    }
                }
                start = range.end;
            }
            if let Some(scanner) = scanner {
                let scanned = scanner.finish();
                ensure!(
                    scanned.gaps.is_subset(&evidence.inputs.gaps),
                    "input receipt inspection gaps omitted"
                );
                uniqueness.receipts(&source.descriptor, &scanned.receipts, &scanned.ambiguous)?;
                let receipts = scanned.receipts;
                for input in evidence
                    .inputs
                    .bindings
                    .iter()
                    .filter(|input| input.input.range.source_id == source.id)
                {
                    let matching: Vec<_> = receipts
                        .iter()
                        .filter(|receipt| {
                            receipt.node == input.node
                                && receipt.native_id == input.native_id
                                && receipt.input == input.input
                        })
                        .collect();
                    ensure!(
                        matching.len() == 1,
                        "execution input receipt unavailable or ambiguous"
                    );
                    let receipt = matching[0];
                    let (_, producer) = self.membership_record(db, &input.producer).await?;
                    let claim: crate::session_evidence::adapter_bridge::Observation =
                        serde_json::from_value(producer.input.raw["payload"].clone())?;
                    ensure!(
                        crate::session_evidence::native_inputs::matches(&claim.event, receipt),
                        "execution producer/native identity mismatch"
                    );
                    ensure!(
                        serde_json::to_value(
                            receipt
                                .codex_history
                                .as_ref()
                                .and_then(|history| history.lifecycle.as_ref())
                        )? == serde_json::to_value(
                            input
                                .codex_history
                                .as_ref()
                                .and_then(|history| history.lifecycle.as_ref())
                        )?,
                        "execution lifecycle selection changed"
                    );
                    ensure!(
                        serde_json::to_value(&receipt.execution)?
                            == serde_json::to_value(&input.execution)?
                            && serde_json::to_value(&receipt.acknowledgements)?
                                == serde_json::to_value(&input.acknowledgements)?,
                        "execution input receipt changed"
                    );
                }
            }
            if let Some(scanner) = rollout {
                let parsed = scanner.finish();
                let candidate = evidence.descendant_sources.iter().any(|range| {
                    range.source_id == source.id && range.generation == prefix.range.generation
                });
                if candidate {
                    ensure!(
                        parsed.gaps.is_empty(),
                        "descendant candidate source invalid"
                    );
                }
                for input in &evidence.inputs.bindings {
                    let Some(history) = &input.codex_history else {
                        continue;
                    };
                    let Some(expected) = &history.execution else {
                        ensure!(
                            history.source.is_none(),
                            "selected rollout lacks own execution"
                        );
                        continue;
                    };
                    if expected
                        .records
                        .first()
                        .is_none_or(|reference| reference.range.source_id != source.id)
                    {
                        continue;
                    }
                    let run = parsed
                        .runs
                        .iter()
                        .find(|run| run.turn_id == input.native_id)
                        .context("input own rollout turn missing")?;
                    ensure!(
                        serde_json::to_value(run)? == serde_json::to_value(expected)?
                            && run
                                .contexts
                                .iter()
                                .map(|context| &context.record)
                                .eq(history.turn_contexts.iter()),
                        "input own rollout execution changed"
                    );
                    if let Some(identity) = &history.source {
                        ensure!(
                            parsed.gaps.is_empty()
                                && identity.node == input.node
                                && identity.id == source.id
                                && parsed.metadata.as_ref() == Some(&identity.metadata),
                            "input own rollout source mismatch"
                        );
                    }
                }
                for child in evidence
                    .descendants
                    .iter()
                    .filter(|child| child.ancestry[0].range.source_id == source.id)
                {
                    ensure!(
                        parsed.gaps.is_empty()
                            && source.descriptor.node.as_ref() == Some(&child.node),
                        "descendant source execution invalid"
                    );
                    let run = parsed
                        .runs
                        .iter()
                        .find(|run| run.turn_id == child.execution.turn_id)
                        .context("descendant own turn missing")?;
                    ensure!(
                        serde_json::to_value(run)? == serde_json::to_value(&child.execution)?,
                        "descendant own turn changed"
                    );
                }
                if candidate {
                    for child in &evidence.descendants {
                        if source.descriptor.node.as_ref() == Some(&child.node)
                            && child.ancestry[0].range.source_id != source.id
                        {
                            ensure!(
                                !parsed
                                    .runs
                                    .iter()
                                    .any(|run| run.turn_id == child.execution.turn_id),
                                "descendant has competing own execution source"
                            );
                        }
                    }
                }
            }
        }
        ensure!(
            references.is_empty(),
            "execution evidence outside inspected prefixes"
        );
        for input in &evidence.inputs.bindings {
            self.input_attachment_on(db, input, evidence).await?;
            let operation = self
                .prompt_operation(
                    db,
                    &PromptOperation::key(&input.origin.controller_id, &input.origin.operation_id)?,
                )
                .await?
                .context("execution prompt missing")?;
            ensure!(
                operation.attempt_id == evidence.attempt_id
                    && operation.session == evidence.session,
                "execution belongs to another attempt"
            );
            let rows = range_records(db, &self.owner_key, &input.producer.range).await?;
            let record = rows.first().context("execution producer missing")?;
            let row = source_entity::Entity::find_by_id(&record.source_id)
                .filter(source_entity::Column::Owner.eq(&self.owner_key))
                .one(db)
                .await?
                .context("execution producer source missing")?;
            let observation = self
                .verify_bridge_observation(db, &decode_source(row)?, record)
                .await?;
            ensure!(
                observation.origin == input.origin
                    && crate::session_evidence::native_inputs::target(&observation.event)
                        == Some(input.native_id.as_str()),
                "execution producer claim mismatch"
            );
        }
        uniqueness.verify(evidence)?;
        let mut spawn_variants = BTreeMap::<NodeKey, BTreeSet<Option<NodeKey>>>::new();
        for range in &evidence.descendant_sources {
            let rows = range_records(
                db,
                &self.owner_key,
                &SourceRange {
                    end: 1,
                    ..range.clone()
                },
            )
            .await?;
            let metadata = rows.first().context("candidate metadata missing")?;
            let (source, _) = self
                .membership_record(db, &RecordRef::from_record(metadata)?)
                .await?;
            ensure!(
                source.format == SourceFormat::CodexRollout,
                "foreign descendant candidate source"
            );
            let node = source.node.context("descendant candidate owner missing")?;
            let parent = crate::session_evidence::service::membership::spawn_parent(
                &node,
                &metadata.input.raw["payload"],
            )?;
            spawn_variants.entry(node).or_default().insert(parent);
        }
        for child in &evidence.descendants {
            let root = evidence
                .inputs
                .bindings
                .iter()
                .find(|input| input.input == child.root_input)
                .context("descendant input missing")?;
            let mut node = child.node.clone();
            let mut seen = BTreeSet::new();
            for metadata in &child.ancestry {
                ensure!(
                    evidence
                        .descendant_sources
                        .iter()
                        .any(|range| range.source_id == metadata.range.source_id
                            && range.generation == metadata.range.generation
                            && range.start == 0
                            && range.end >= metadata.range.end),
                    "descendant ancestry outside candidate inspection"
                );
                ensure!(seen.insert(node.clone()), "cyclic descendant ancestry");
                let (source, record) = self.membership_record(db, metadata).await?;
                ensure!(
                    metadata.range.start == 0
                        && source.format == SourceFormat::CodexRollout
                        && source.node.as_ref() == Some(&node)
                        && record.input.raw["type"] == "session_meta"
                        && record.input.raw["payload"]["id"].as_str() == Some(&node.native_id),
                    "spawn metadata identity mismatch"
                );
                let parent = crate::session_evidence::service::membership::spawn_parent(
                    &node,
                    &record.input.raw["payload"],
                )?
                .context("spawn parent missing")?;
                ensure!(
                    spawn_variants.get(&node).is_some_and(
                        |parents| parents.len() == 1 && parents.contains(&Some(parent.clone()))
                    ),
                    "descendant has conflicting spawn ancestry"
                );
                node = parent;
            }
            ensure!(
                node == root.node,
                "descendant ancestry does not reach input owner"
            );
        }
        Ok(())
    }
}

fn membership_ranges(evidence: &AttemptExecutions) -> Result<Vec<SourceRange>> {
    let mut ranges = evidence.inputs.inspected.clone();
    for range in &evidence.descendant_sources {
        ensure!(range.start == 0, "descendant candidate prefix missing");
        ranges.push(range.clone());
    }
    for input in &evidence.inputs.bindings {
        if let Some(history) = &input.codex_history {
            ranges.extend(history.inspected.iter().cloned());
        }
    }
    ranges.extend(
        evidence
            .references()?
            .into_values()
            .map(|reference| reference.range),
    );
    ranges.sort_by(|a, b| {
        (&a.source_id, &a.generation, a.start, a.end).cmp(&(
            &b.source_id,
            &b.generation,
            b.start,
            b.end,
        ))
    });
    let mut merged: Vec<SourceRange> = vec![];
    for range in ranges {
        range.validate()?;
        if let Some(last) = merged.last_mut()
            && last.source_id == range.source_id
            && last.generation == range.generation
            && last.end >= range.start
        {
            last.end = last.end.max(range.end);
        } else {
            merged.push(range);
        }
    }
    ensure!(
        merged.len() <= MAX_GRAPH_ITEMS
            && merged
                .iter()
                .map(|range| range.end - range.start)
                .sum::<u64>()
                <= MAX_RECORDS as u64,
        "execution inspection budget exceeded"
    );
    Ok(merged)
}

async fn prefix_digest(
    db: &impl ConnectionTrait,
    owner: &str,
    range: &SourceRange,
) -> Result<String> {
    let mut digest = Sha256::new();
    let mut start = range.start;
    while start < range.end {
        let page = SourceRange {
            start,
            end: (start + RECORD_PAGE_SIZE).min(range.end),
            ..range.clone()
        };
        let rows = range_records(db, owner, &page).await?;
        ensure!(
            rows.len() as u64 == page.end - page.start,
            "execution inspection records missing"
        );
        for record in rows {
            digest.update(record.id.as_bytes());
            digest.update(record.digest.as_bytes());
        }
        start = page.end;
    }
    Ok(digest
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect())
}
