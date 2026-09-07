//! Codex turn bookends from verified native identities. These are observed
//! executions, not task membership, live-agent state or settled checkpoints.

use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{FactKind, NativeFact};
use crate::session_evidence::types::{Harness, MAX_GRAPH_ITEMS, NodeKey, RecordRef, SourceFormat};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOrigin {
    AppServer,
    LocalRollout,
    UnverifiedHistory,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunStart {
    pub record: RecordRef,
    pub origin: RunOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RunOutcome {
    Completed,
    Failed,
    Interrupted,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RunTermination {
    pub record: RecordRef,
    pub outcome: RunOutcome,
    pub abort_reason: Option<String>,
    pub origin: RunOrigin,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CodexRun {
    pub node: NodeKey,
    pub turn_id: String,
    pub starts: Vec<RunStart>,
    pub terminations: Vec<RunTermination>,
    /// An observed terminal outcome. This never certifies task readiness.
    pub outcome: Option<RunOutcome>,
    pub gaps: BTreeSet<String>,
}

pub(crate) fn summarize(facts: &[NativeFact]) -> (Vec<CodexRun>, BTreeSet<String>) {
    let local_sources: BTreeSet<_> = facts
        .iter()
        .filter_map(|fact| {
            let record = fact.record.as_ref()?;
            (fact.source_format == Some(SourceFormat::CodexRollout)
                && matches!(fact.event, FactKind::HistorySource { inherited: false })
                && record.range.start == 0)
                .then_some((
                    record.range.source_id.as_str(),
                    record.range.generation.as_str(),
                ))
        })
        .collect();
    let mut runs = BTreeMap::<(NodeKey, String), CodexRun>::new();
    let mut gaps = BTreeSet::new();
    let mut seen = BTreeSet::new();
    for fact in facts {
        let Some(node) = fact
            .node
            .as_ref()
            .filter(|node| node.harness == Harness::Codex)
        else {
            continue;
        };
        let (id, terminal) = match &fact.event {
            FactKind::RunStarted { run_id } => (run_id, None),
            FactKind::RunFinished { run_id, status } => {
                let outcome = match status.as_str() {
                    "completed" => RunOutcome::Completed,
                    "failed" => RunOutcome::Failed,
                    "interrupted" => RunOutcome::Interrupted,
                    _ => {
                        gaps.insert("native_run_terminal_unsupported".into());
                        continue;
                    }
                };
                (run_id, Some((outcome, None)))
            }
            FactKind::RunAborted {
                run_id: Some(id),
                reason,
            } => (id, Some((RunOutcome::Interrupted, Some(reason.clone())))),
            FactKind::RunAborted { run_id: None, .. } => {
                gaps.insert("native_run_abort_identity_unavailable".into());
                continue;
            }
            _ => continue,
        };
        let Some(record) = &fact.record else {
            gaps.insert("native_run_boundary_unavailable".into());
            continue;
        };
        if !seen.insert(record.record_id.clone()) {
            continue;
        }
        let key = (node.clone(), id.clone());
        if !runs.contains_key(&key) && runs.len() >= MAX_GRAPH_ITEMS {
            gaps.insert("native_run_limit".into());
            continue;
        }
        let run = runs.entry(key).or_insert_with(|| CodexRun {
            node: node.clone(),
            turn_id: id.clone(),
            starts: vec![],
            terminations: vec![],
            outcome: None,
            gaps: BTreeSet::new(),
        });
        let origin = match fact.source_format {
            Some(SourceFormat::CodexAppServer) => RunOrigin::AppServer,
            Some(SourceFormat::CodexRollout)
                if local_sources.contains(&(
                    record.range.source_id.as_str(),
                    record.range.generation.as_str(),
                )) =>
            {
                RunOrigin::LocalRollout
            }
            _ => RunOrigin::UnverifiedHistory,
        };
        if origin == RunOrigin::UnverifiedHistory {
            run.gaps.insert("native_run_execution_unverified".into());
        }
        if let Some((outcome, abort_reason)) = terminal {
            run.terminations.push(RunTermination {
                record: record.clone(),
                outcome,
                abort_reason,
                origin,
            });
        } else {
            run.starts.push(RunStart {
                record: record.clone(),
                origin,
            });
        }
    }
    for run in runs.values_mut() {
        // IDs join multiple native observations; source order only compares
        // records in the same generation. Receive timestamps never order two
        // processes or decide which child execution an agent call resumed.
        run.starts
            .sort_by(|a, b| record_order(&a.record).cmp(&record_order(&b.record)));
        run.terminations
            .sort_by(|a, b| record_order(&a.record).cmp(&record_order(&b.record)));
        if !run
            .starts
            .iter()
            .any(|start| start.origin != RunOrigin::UnverifiedHistory)
        {
            run.gaps.insert("native_run_start_unavailable".into());
        }
        let outcomes: BTreeSet<_> = run
            .terminations
            .iter()
            .filter(|end| end.origin != RunOrigin::UnverifiedHistory)
            .map(|end| end.outcome)
            .collect();
        if outcomes.len() > 1 {
            run.gaps.insert("native_run_terminal_conflict".into());
        } else {
            run.outcome = outcomes.first().copied();
        }
        let mut first_ends = BTreeMap::<(&str, &str), u64>::new();
        for end in run
            .terminations
            .iter()
            .filter(|end| end.origin != RunOrigin::UnverifiedHistory)
        {
            let range = &end.record.range;
            first_ends
                .entry((&range.source_id, &range.generation))
                .and_modify(|sequence| *sequence = (*sequence).min(range.start))
                .or_insert(range.start);
        }
        if run.starts.iter().any(|start| {
            if start.origin == RunOrigin::UnverifiedHistory {
                return false;
            }
            let start = &start.record;
            first_ends
                .get(&(
                    start.range.source_id.as_str(),
                    start.range.generation.as_str(),
                ))
                .is_some_and(|end| start.range.start >= *end)
        }) {
            run.gaps.insert("native_run_boundary_order_invalid".into());
            run.outcome = None;
        }
        gaps.extend(run.gaps.iter().cloned());
    }
    (runs.into_values().collect(), gaps)
}

fn record_order(record: &RecordRef) -> (&str, &str, u64) {
    (
        &record.range.source_id,
        &record.range.generation,
        record.range.start,
    )
}

#[cfg(test)]
mod tests;
