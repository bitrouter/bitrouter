//! Join original prompt receipts to own native executions before publishing
//! attempt membership. Inherited context never supplies descendant executions.

use super::*;
use crate::session_evidence::execution::rollout_runs::RolloutExecutions;
use crate::session_evidence::membership::{AttemptExecutions, DescendantExecution};
use crate::session_evidence::native_inputs::NativeInputEvidence;
use crate::session_evidence::types::{Attempt, MAX_OBJECT_BYTES};

struct Own<'a> {
    node: &'a NodeKey,
    executions: &'a RolloutExecutions,
    parent: Option<NodeKey>,
}

impl ControllerEvidence {
    pub(super) async fn membership_stamps(
        &self,
        gaps: &mut BTreeSet<String>,
    ) -> Result<
        BTreeMap<String, Option<crate::session_evidence::store::tasks::membership::PointerStamp>>,
    > {
        let mut namespaces = BTreeSet::from([self.collector.root().namespace.clone()]);
        {
            let state = self.state.lock().await;
            namespaces.extend(
                state
                    .roots
                    .values()
                    .map(|root| root.collector.root().namespace.clone()),
            );
            namespaces.extend(
                state
                    .snapshot
                    .attempts
                    .iter()
                    .map(|attempt| attempt.session.namespace.clone()),
            );
            namespaces.extend(
                state
                    .snapshot
                    .histories
                    .iter()
                    .map(|history| history.node.namespace.clone()),
            );
        }
        let (sessions, task_gaps) = self
            .store
            .task_sessions(self.collector.root().harness, &namespaces)
            .await?;
        gaps.extend(task_gaps);
        // Keep current attempts visible before spending the remaining slots on
        // archives. Rotate the bounded archive page across reconcile passes.
        let mut targets = BTreeSet::new();
        for session in &sessions {
            match self.store.active_attempt(session).await {
                Ok(Some(attempt)) if attempt.phase != super::super::types::AttemptPhase::Ready => {
                    targets.insert(attempt.id);
                }
                Ok(_) => {}
                Err(error) => {
                    tracing::warn!(%error, "execution current task unavailable");
                    gaps.insert("native_attempt_membership_inventory_pending".into());
                }
            }
        }
        let capacity = MAX_GRAPH_ITEMS.saturating_sub(targets.len());
        let after = self.state.lock().await.membership_after.clone();
        let mut archived = BTreeSet::new();
        let mut backlog = false;
        for session in &sessions {
            match self.store.unsettled_attempts(session).await {
                Ok(attempts) => {
                    for attempt in attempts {
                        if targets.contains(&attempt.id) {
                            continue;
                        }
                        archived.insert((
                            after.as_ref().is_some_and(|after| attempt.id <= *after),
                            attempt.id,
                        ));
                        if archived.len() > capacity {
                            archived.pop_last();
                            backlog = true;
                        }
                    }
                }
                Err(error) => {
                    tracing::warn!(%error, "execution target task unavailable");
                    gaps.insert("native_attempt_membership_inventory_pending".into());
                }
            }
        }
        if backlog {
            gaps.insert("native_attempt_membership_inventory_pending".into());
        }
        if let Some((_, last)) = archived.last() {
            self.state.lock().await.membership_after = Some(last.clone());
        }
        targets.extend(archived.into_iter().map(|(_, id)| id));
        let mut stamps = BTreeMap::new();
        for attempt in targets {
            match self.store.execution_pointer_stamp(&attempt).await {
                Ok(stamp) => {
                    stamps.insert(attempt, stamp);
                }
                Err(error) => {
                    tracing::warn!(%error, "execution target pointer unavailable");
                    gaps.insert("native_attempt_membership_inventory_pending".into());
                }
            }
        }
        Ok(stamps)
    }

    pub(super) async fn attempt_executions(
        &self,
        attempts: &mut [Attempt],
        inputs: &BTreeMap<String, NativeInputEvidence>,
        histories: &[ResolvedHistory],
        stamps: &BTreeMap<
            String,
            Option<crate::session_evidence::store::tasks::membership::PointerStamp>,
        >,
        gaps: &mut BTreeSet<String>,
    ) -> BTreeMap<String, AttemptExecutions> {
        let mut output = BTreeMap::new();
        let mut budget = MAX_OBJECT_BYTES;
        for attempt in attempts {
            let Some(stamp) = stamps.get(&attempt.id) else {
                gaps.insert("native_attempt_membership_inventory_pending".into());
                continue;
            };
            let Some(inputs) = inputs.get(&attempt.id) else {
                gaps.insert("native_attempt_membership_unavailable".into());
                continue;
            };
            let result = async {
                let bytes = serde_json::to_vec(inputs)?.len();
                ensure!(bytes <= budget, "attempt membership materialization limit");
                budget -= bytes;
                let mut evidence = AttemptExecutions::new(
                    attempt.id.clone(),
                    attempt.revision,
                    attempt.session.clone(),
                    inputs.clone(),
                );
                self.descendant_executions(&mut evidence, histories, &mut budget)
                    .await?;
                let (updated, evidence) = self
                    .store
                    .record_attempt_executions(attempt, stamp.as_ref(), evidence)
                    .await?;
                anyhow::Ok((updated, evidence))
            }
            .await;
            match result {
                Ok((updated, evidence)) => {
                    *attempt = updated;
                    if attempt.members.is_empty() {
                        gaps.insert("native_attempt_membership_unavailable".into());
                    }
                    gaps.extend(evidence.gaps.iter().cloned());
                    output.insert(attempt.id.clone(), evidence);
                }
                Err(error) => {
                    tracing::warn!(%error, "attempt execution membership could not be committed");
                    gaps.insert("native_attempt_membership_invalid".into());
                }
            }
        }
        output
    }

    async fn descendant_executions(
        &self,
        evidence: &mut AttemptExecutions,
        histories: &[ResolvedHistory],
        budget: &mut usize,
    ) -> Result<()> {
        if evidence.session.harness != Harness::Codex {
            return Ok(());
        }
        let mut pending: Vec<_> = histories.iter().collect();
        let mut own = BTreeMap::new();
        let mut blocked = BTreeSet::new();
        let mut invalid_runs = BTreeSet::new();
        let mut visited = 0;
        while let Some(history) = pending.pop() {
            visited += 1;
            ensure!(
                visited <= MAX_GRAPH_ITEMS,
                "attempt history inspection limit"
            );
            // A dependency's checkpoint is inherited context, not another run.
            pending.extend(&history.variants);
            if history.node.namespace != evidence.session.namespace {
                continue;
            }
            if history.source.is_none()
                && history.gaps.iter().any(|gap| {
                    gap.starts_with("rollout_inventory_")
                        || matches!(
                            gap.as_str(),
                            "native_history_discovery_failed" | "native_history_candidate_invalid"
                        )
                })
            {
                blocked.insert(history.node.clone());
                evidence
                    .gaps
                    .insert("native_attempt_spawn_ambiguous".into());
            }
            let Some(executions) = &history.codex_executions else {
                if history.source.is_some() {
                    blocked.insert(history.node.clone());
                }
                continue;
            };
            let Some(metadata) = &executions.metadata else {
                blocked.insert(history.node.clone());
                continue;
            };
            if !executions.gaps.is_empty() {
                blocked.insert(history.node.clone());
            }
            for run in &executions.runs {
                if !run.gaps.is_empty() {
                    invalid_runs.insert((history.node.clone(), run.turn_id.clone()));
                }
            }
            let source = self
                .store
                .source(&metadata.range.source_id)
                .await?
                .context("descendant source missing")?;
            ensure!(
                source.descriptor.node.as_ref() == Some(&history.node)
                    && source.descriptor.format == SourceFormat::CodexRollout,
                "descendant source owner mismatch"
            );
            let rows = self.store.records(&metadata.range).await?;
            let record = rows.first().context("descendant metadata missing")?;
            ensure!(
                RecordRef::from_record(record)? == *metadata
                    && metadata.range.start == 0
                    && record.input.raw["type"] == "session_meta"
                    && record.input.raw["payload"]["id"].as_str() == Some(&history.node.native_id),
                "descendant metadata mismatch"
            );
            let parent = match spawn_parent(&history.node, &record.input.raw["payload"]) {
                Ok(parent) => parent,
                Err(_) => {
                    blocked.insert(history.node.clone());
                    evidence
                        .gaps
                        .insert("native_attempt_spawn_ambiguous".into());
                    continue;
                }
            };
            own.insert(
                (
                    metadata.range.source_id.clone(),
                    metadata.range.generation.clone(),
                ),
                Own {
                    node: &history.node,
                    executions,
                    parent,
                },
            );
        }
        let mut candidates = BTreeMap::<(NodeKey, String), Vec<DescendantExecution>>::new();
        for history in own.values() {
            if !history.executions.gaps.is_empty() || blocked.contains(history.node) {
                continue;
            }
            for run in &history.executions.runs {
                if invalid_runs.contains(&(history.node.clone(), run.turn_id.clone())) {
                    continue;
                }
                let Some(root_turn) = &run.root_turn_id else {
                    continue;
                };
                if !run.gaps.is_empty() {
                    continue;
                }
                let roots: Vec<_> = evidence
                    .inputs
                    .bindings
                    .iter()
                    .filter(|input| {
                        input.native_id == *root_turn
                            && input.node != *history.node
                            && input.codex_history.as_ref().is_some_and(|selected| {
                                selected.source.is_some()
                                    && selected.gaps.is_empty()
                                    && selected.execution.as_ref().is_some_and(|root| {
                                        root.root_turn_id.as_ref() == Some(&input.native_id)
                                    })
                            })
                    })
                    .collect();
                if roots.is_empty() {
                    continue;
                }
                if roots.len() != 1 {
                    evidence
                        .gaps
                        .insert("native_attempt_root_turn_ambiguous".into());
                    continue;
                }
                let root = roots[0];
                let ancestry = match ancestry(history, &root.node, &own, &blocked) {
                    Some(ancestry) => ancestry,
                    None => {
                        evidence
                            .gaps
                            .insert("native_attempt_spawn_unverified".into());
                        continue;
                    }
                };
                let bytes = serde_json::to_vec(run)?.len()
                    + serde_json::to_vec(&ancestry)?.len()
                    + serde_json::to_vec(&root.input)?.len()
                    + serde_json::to_vec(history.node)?.len();
                ensure!(
                    bytes <= *budget,
                    "descendant membership materialization limit"
                );
                *budget -= bytes;
                let key = (history.node.clone(), run.turn_id.clone());
                let candidates = candidates.entry(key).or_default();
                ensure!(candidates.len() < 2, "descendant execution ambiguity limit");
                candidates.push(DescendantExecution {
                    node: history.node.clone(),
                    root_input: root.input.clone(),
                    ancestry,
                    execution: run.clone(),
                });
            }
        }
        for candidates in candidates.into_values() {
            if candidates.len() != 1 {
                evidence
                    .gaps
                    .insert("native_attempt_descendant_source_ambiguous".into());
                continue;
            }
            let child = candidates
                .into_iter()
                .next()
                .context("descendant candidate missing")?;
            let source = own
                .get(&(
                    child.ancestry[0].range.source_id.clone(),
                    child.ancestry[0].range.generation.clone(),
                ))
                .context("descendant history missing")?;
            ensure!(
                source.executions.inspected.is_some(),
                "descendant source prefix missing"
            );
            evidence.descendants.push(child);
        }
        let mut lineage_nodes = BTreeSet::new();
        for child in &evidence.descendants {
            for metadata in &child.ancestry {
                let source = own
                    .get(&(
                        metadata.range.source_id.clone(),
                        metadata.range.generation.clone(),
                    ))
                    .context("ancestry source missing")?;
                lineage_nodes.insert(source.node.clone());
            }
        }
        for source in own
            .values()
            .filter(|source| lineage_nodes.contains(source.node))
        {
            if let Some(range) = &source.executions.inspected {
                evidence.descendant_sources.push(range.clone());
            }
        }
        Ok(())
    }
}

pub(crate) fn spawn_parent(node: &NodeKey, payload: &Value) -> Result<Option<NodeKey>> {
    // Both metadata fields describe actual spawning, independently of logical
    // fork/history_base and mutable session-tree grouping.
    // https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/protocol/src/protocol.rs
    let mut parent = None;
    for value in [
        payload.get("parent_thread_id"),
        payload.pointer("/source/subagent/thread_spawn/parent_thread_id"),
    ]
    .into_iter()
    .flatten()
    .filter(|value| !value.is_null())
    {
        let id = value.as_str().context("invalid spawn parent")?;
        super::super::types::identifier(id)?;
        ensure!(
            parent.is_none_or(|old| old == id) && id != node.native_id,
            "conflicting spawn parents"
        );
        parent = Some(id);
    }
    Ok(parent.map(|id| NodeKey {
        native_id: id.into(),
        ..node.clone()
    }))
}

fn ancestry<'a>(
    child: &'a Own<'a>,
    root: &NodeKey,
    sources: &'a BTreeMap<(String, String), Own<'a>>,
    blocked: &BTreeSet<NodeKey>,
) -> Option<Vec<RecordRef>> {
    let mut current = child;
    let mut seen = BTreeSet::new();
    let mut proof = vec![];
    while current.node != root {
        if blocked.contains(current.node) {
            return None;
        }
        if seen.len() == 64 || !seen.insert(current.node.clone()) {
            return None;
        }
        let parent = current.parent.as_ref()?;
        // Any conflicting original metadata blocks choosing a convenient path.
        if sources
            .values()
            .any(|other| other.node == current.node && other.parent.as_ref() != Some(parent))
        {
            return None;
        }
        proof.push(current.executions.metadata.clone()?);
        if parent == root {
            return Some(proof);
        }
        current = sources.values().find(|other| other.node == parent)?;
    }
    None
}
