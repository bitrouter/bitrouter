//! Corroborate connection-local rollout selections against owned file sources.

use super::*;
use crate::session_evidence::execution::rollout_runs::{self, OwnHistory};
use crate::session_evidence::native_inputs::rollouts::CodexHistoryEvidence;
use crate::session_evidence::store::rollouts::RolloutIdentity;

#[derive(Default)]
struct Inspection {
    identity: Option<RolloutIdentity>,
    turns: BTreeMap<String, Vec<RecordRef>>,
    ranges: Vec<SourceRange>,
    gaps: BTreeSet<String>,
    runs: BTreeMap<String, rollout_runs::RolloutRun>,
}

impl ControllerEvidence {
    pub(super) async fn corroborate_rollouts(
        &self,
        group: &mut Group,
        budget: &mut u64,
        details_budget: &mut usize,
    ) -> Result<()> {
        let mut targets = BTreeMap::<(NodeKey, String), BTreeSet<String>>::new();
        for binding in &mut group.bindings {
            let Some(history) = &mut binding.codex_history else {
                continue;
            };
            let Some(lifecycle) = &history.lifecycle else {
                continue;
            };
            if !within_root(Path::new(&lifecycle.path), &group.native_root) {
                history
                    .gaps
                    .insert("native_input_rollout_path_outside_root".into());
                continue;
            }
            targets
                .entry((binding.node.clone(), lifecycle.rollout_id.clone()))
                .or_default()
                .insert(binding.native_id.clone());
        }
        if targets.is_empty() {
            return Ok(());
        }
        let nodes: BTreeSet<_> = targets.keys().map(|(node, _)| node).collect();
        let mut found =
            BTreeMap::<(NodeKey, String), Vec<(RegisteredSource, RolloutIdentity)>>::new();
        let mut inventory_gaps = BTreeSet::new();
        let mut invalid_nodes = BTreeSet::new();
        let mut after = None;
        loop {
            // Inventory and raw inspections share the controller pass budget.
            if *budget == 0 {
                inventory_gaps.insert("native_input_rollout_inventory_limit".into());
                break;
            }
            let limit = 128.min(*budget);
            let page = self.store.source_inventory(after.as_deref(), limit).await?;
            *budget -= page.len() as u64;
            let done = page.len() < limit as usize;
            for (id, source) in page {
                after = Some(id);
                let source = match source {
                    Ok(source) => source,
                    Err(_) => {
                        inventory_gaps.insert("native_input_rollout_inventory_invalid".into());
                        continue;
                    }
                };
                if source.descriptor.format != SourceFormat::CodexRollout {
                    continue;
                }
                let Some(node) = &source.descriptor.node else {
                    continue;
                };
                if !nodes.contains(node) {
                    continue;
                }
                if *budget == 0 {
                    inventory_gaps.insert("native_input_rollout_inventory_limit".into());
                    break;
                }
                *budget -= 1;
                match self.store.rollout_identity(&source.id).await {
                    Ok(Some(identity)) => {
                        let key = (node.clone(), identity.rollout_id.clone());
                        if targets.contains_key(&key) {
                            let candidates = found.entry(key).or_default();
                            // Two distinct sources suffice to prove ambiguity.
                            // Still inspect the rest, without retaining copies.
                            if candidates.len() < 2 {
                                let bytes = serde_json::to_vec(&(&source, &identity))?.len();
                                if bytes <= *details_budget {
                                    *details_budget -= bytes;
                                    candidates.push((source, identity));
                                } else {
                                    inventory_gaps.insert(
                                        "native_input_rollout_materialization_limit".into(),
                                    );
                                }
                            }
                        }
                    }
                    _ => {
                        invalid_nodes.insert(node.clone());
                    }
                }
            }
            if done {
                break;
            }
        }
        let mut inspected = BTreeMap::new();
        for (key, turns) in targets {
            let mut inspection = Inspection::default();
            inspection.gaps.extend(inventory_gaps.iter().cloned());
            if invalid_nodes.contains(&key.0) {
                inspection
                    .gaps
                    .insert("native_input_rollout_identity_invalid".into());
            }
            let candidates = found.remove(&key).unwrap_or_default();
            if candidates.len() == 1 {
                for (source, identity) in candidates {
                    if let Err(error) = self
                        .inspect_rollout(
                            &source,
                            identity,
                            &turns,
                            budget,
                            details_budget,
                            &mut inspection,
                        )
                        .await
                    {
                        tracing::warn!(%error, "native input rollout records invalid");
                        inspection.identity = None;
                        inspection
                            .gaps
                            .insert("native_input_rollout_records_invalid".into());
                    }
                }
            } else {
                inspection.gaps.insert(
                    if candidates.is_empty() {
                        "native_input_rollout_source_unavailable"
                    } else {
                        "native_input_rollout_source_ambiguous"
                    }
                    .into(),
                );
            }
            inspected.insert(key, inspection);
        }
        for binding in &mut group.bindings {
            let Some(history) = &mut binding.codex_history else {
                continue;
            };
            let Some(lifecycle) = &history.lifecycle else {
                continue;
            };
            if !within_root(Path::new(&lifecycle.path), &group.native_root) {
                continue;
            }
            if let Some(inspection) =
                inspected.get(&(binding.node.clone(), lifecycle.rollout_id.clone()))
            {
                history.gaps.extend(inspection.gaps.iter().cloned());
                let contexts = inspection
                    .turns
                    .get(&binding.native_id)
                    .map(Vec::as_slice)
                    .unwrap_or_default();
                let execution = inspection.runs.get(&binding.native_id);
                if contexts.is_empty() {
                    history
                        .gaps
                        .insert("native_input_rollout_turn_unobserved".into());
                }
                // Reserve the borrowed fields plus object keys before cloning
                // a shared inspection into another producer binding.
                let bytes = serde_json::to_vec(&(
                    &history.lifecycle,
                    &inspection.identity,
                    contexts,
                    execution,
                    &inspection.ranges,
                    &history.gaps,
                ))?
                .len()
                    + 128;
                ensure!(
                    bytes <= *details_budget,
                    "native rollout materialization limit"
                );
                *details_budget -= bytes;
                let mut checked = CodexHistoryEvidence {
                    lifecycle: history.lifecycle.clone(),
                    source: inspection.identity.clone(),
                    turn_contexts: contexts.to_vec(),
                    execution: execution.cloned(),
                    inspected: inspection.ranges.clone(),
                    gaps: history.gaps.clone(),
                };
                if !checked.gaps.is_empty() {
                    checked.source = None;
                }
                *history = checked;
            }
        }
        Ok(())
    }

    async fn inspect_rollout(
        &self,
        source: &RegisteredSource,
        identity: RolloutIdentity,
        turns: &BTreeSet<String>,
        budget: &mut u64,
        details_budget: &mut usize,
        inspection: &mut Inspection,
    ) -> Result<()> {
        if source.cursor.generation != identity.metadata.range.generation {
            inspection
                .gaps
                .insert("native_input_rollout_replaced".into());
            return Ok(());
        }
        let end = source.cursor.next_sequence;
        ensure!(end > 0, "native rollout prefix missing");
        if end > *budget {
            inspection
                .gaps
                .insert("native_input_rollout_inspection_limit".into());
            return Ok(());
        }
        *budget -= end;
        let mut start = 0;
        let mut ownership = OwnHistory::Unknown;
        let mut executions = rollout_runs::Scanner::new(identity.node.clone());
        while start < end {
            let range = SourceRange {
                source_id: source.id.clone(),
                generation: source.cursor.generation.clone(),
                start,
                end: (start + RECORD_PAGE_SIZE).min(end),
            };
            let records = self.store.records(&range).await?;
            ensure!(
                records.len() as u64 == range.end - range.start,
                "native rollout records missing"
            );
            for record in records {
                executions.push(&record, details_budget);
                let raw = &record.input.raw;
                if record.input.sequence == 0 {
                    ensure!(
                        RecordRef::from_record(&record)? == identity.metadata,
                        "native rollout metadata changed"
                    );
                    let payload = &raw["payload"];
                    ownership = OwnHistory::read(payload)?;
                }
                if raw["type"] != "turn_context" {
                    continue;
                }
                let Some(turn) = raw
                    .pointer("/payload/turn_id")
                    .and_then(Value::as_str)
                    .filter(|turn| turns.contains(*turn))
                else {
                    continue;
                };
                if !ownership.owns(&record)? {
                    continue;
                }
                let references = inspection.turns.entry(turn.into()).or_default();
                ensure!(
                    references.len() < MAX_GRAPH_ITEMS,
                    "native turn context limit"
                );
                let reference = RecordRef::from_record(&record)?;
                let bytes = serde_json::to_vec(&reference)?.len();
                if bytes > *details_budget {
                    inspection
                        .gaps
                        .insert("native_input_rollout_materialization_limit".into());
                    return Ok(());
                }
                *details_budget -= bytes;
                references.push(reference);
            }
            start = range.end;
        }
        inspection.ranges.push(SourceRange {
            source_id: source.id.clone(),
            generation: source.cursor.generation.clone(),
            start: 0,
            end,
        });
        inspection.identity = Some(identity);
        let executions = executions.finish();
        inspection.gaps.extend(executions.gaps);
        inspection.runs = executions
            .runs
            .into_iter()
            .filter(|run| turns.contains(&run.turn_id))
            .map(|run| (run.turn_id.clone(), run))
            .collect();
        Ok(())
    }
}

fn within_root(path: &Path, root: &Path) -> bool {
    // Both paths were originally captured on this host. Do not reopen a path
    // supplied by native messages or rely on it still existing after recovery.
    if path.components().any(|part| {
        matches!(
            part,
            std::path::Component::ParentDir | std::path::Component::CurDir
        )
    }) {
        return false;
    }
    path.starts_with(root)
        || (root.file_name().is_some_and(|name| name == "sessions")
            && root
                .parent()
                .is_some_and(|profile| path.starts_with(profile.join("archived_sessions"))))
}
