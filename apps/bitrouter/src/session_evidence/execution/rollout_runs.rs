//! Source-local execution observations. Explicit native turn identities select
//! records; a bookend span does not attribute every intervening record.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use serde_json::Value;

use super::runs::RunOutcome;
use crate::session_evidence::types::{
    MAX_GRAPH_ITEMS, MAX_RECORDS, NodeKey, RecordRef, SourceRange, StoredRecord, identifier,
};

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TurnContext {
    pub record: RecordRef,
    pub root_turn_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Termination {
    pub record: RecordRef,
    pub outcome: RunOutcome,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RolloutRun {
    pub turn_id: String,
    pub records: Vec<RecordRef>,
    pub starts: Vec<RecordRef>,
    pub terminations: Vec<Termination>,
    pub contexts: Vec<TurnContext>,
    /// A native attribution claim, not independent proof of spawn ancestry.
    pub root_turn_id: Option<String>,
    /// Original bookends only. Records without a turn id remain unassigned.
    pub observed_span: Option<SourceRange>,
    pub outcome: Option<RunOutcome>,
    pub gaps: BTreeSet<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct RolloutExecutions {
    pub metadata: Option<RecordRef>,
    pub inspected: Option<SourceRange>,
    pub runs: Vec<RolloutRun>,
    /// Own-source records without an explicit supported turn identity. These
    /// may be context, auxiliary work or accounting; they are not discarded.
    pub unassigned: Vec<SourceRange>,
    pub gaps: BTreeSet<String>,
}

#[derive(Debug, Clone, Copy)]
pub(crate) enum OwnHistory {
    All,
    FromOrdinal(u64),
    Unknown,
}

impl OwnHistory {
    pub fn read(metadata: &Value) -> Result<Self> {
        // Logical fork ordinals do not necessarily index a copied child file.
        // Paginated subagents have an explicit own-history boundary; referenced
        // rollouts keep their ancestor records outside the local source.
        // https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/protocol/src/protocol.rs
        if let Some(cut) = metadata
            .get("subagent_history_start_ordinal")
            .filter(|value| !value.is_null())
        {
            return Ok(Self::FromOrdinal(
                cut.as_u64().context("invalid subagent history cut")?,
            ));
        }
        if let Some(base) = metadata
            .get("history_base")
            .filter(|value| !value.is_null())
        {
            return Ok(Self::FromOrdinal(
                base.get("end_ordinal_exclusive")
                    .and_then(Value::as_u64)
                    .context("invalid physical history cut")?,
            ));
        }
        Ok(
            if metadata
                .get("forked_from_id")
                .is_some_and(|value| !value.is_null())
            {
                Self::Unknown
            } else {
                Self::All
            },
        )
    }

    pub fn owns(self, record: &StoredRecord) -> Result<bool> {
        match self {
            Self::All => Ok(true),
            Self::FromOrdinal(cut) => Ok(record
                .input
                .raw
                .get("ordinal")
                .and_then(Value::as_u64)
                .context("own-history ordinal missing")?
                >= cut),
            Self::Unknown => Ok(false),
        }
    }
}

pub(crate) struct Scanner {
    node: NodeKey,
    evidence: RolloutExecutions,
    runs: BTreeMap<String, RolloutRun>,
    ownership: OwnHistory,
    next: u64,
    next_ordinal: Option<u64>,
    disabled: bool,
}

impl Scanner {
    pub fn new(node: NodeKey) -> Self {
        Self {
            node,
            evidence: RolloutExecutions::default(),
            runs: BTreeMap::new(),
            ownership: OwnHistory::Unknown,
            next: 0,
            next_ordinal: None,
            disabled: false,
        }
    }

    pub fn push(&mut self, record: &StoredRecord, bytes: &mut usize) {
        if self.disabled {
            return;
        }
        if let Err(error) = self.read(record, bytes) {
            tracing::debug!(%error, "native rollout execution prefix incomplete");
            self.evidence
                .gaps
                .insert("native_rollout_execution_invalid".into());
            self.disabled = true;
        }
    }

    fn read(&mut self, record: &StoredRecord, bytes: &mut usize) -> Result<()> {
        ensure!(
            record.input.sequence == self.next && self.next < MAX_RECORDS as u64,
            "rollout execution sequence invalid"
        );
        let raw = &record.input.raw;
        let payload = raw.get("payload").context("rollout payload missing")?;
        let reference = RecordRef::from_record(record)?;
        let ordinal = raw
            .get("ordinal")
            .map(|value| value.as_u64().context("invalid ordinal"))
            .transpose()?;
        if self.next == 0 {
            ensure!(
                raw["type"] == "session_meta"
                    && payload["id"].as_str() == Some(&self.node.native_id),
                "rollout execution identity missing"
            );
            self.ownership = OwnHistory::read(payload)?;
            let initial = payload
                .pointer("/history_base/end_ordinal_exclusive")
                .and_then(Value::as_u64)
                .unwrap_or(0);
            ensure!(
                ordinal.is_none_or(|ordinal| ordinal == initial),
                "rollout initial ordinal invalid"
            );
            if matches!(self.ownership, OwnHistory::Unknown) {
                self.evidence
                    .gaps
                    .insert("native_rollout_own_history_unavailable".into());
            }
            reserve(bytes, serde_json::to_vec(&reference)?.len() * 2)?;
            self.evidence.inspected = Some(reference.range.clone());
            self.evidence.metadata = Some(reference.clone());
        } else {
            let inspected = self
                .evidence
                .inspected
                .as_mut()
                .context("rollout prefix missing")?;
            ensure!(
                inspected.source_id == record.source_id
                    && inspected.generation == record.input.generation,
                "rollout execution source changed"
            );
            ensure!(
                ordinal == self.next_ordinal,
                "rollout execution ordinal gap"
            );
            inspected.end = reference.range.end;
        }
        self.next += 1;
        self.next_ordinal = ordinal
            .map(|ordinal| ordinal.checked_add(1).context("rollout ordinal overflow"))
            .transpose()?;
        if record.input.sequence == 0 || !self.ownership.owns(record)? {
            return Ok(());
        }
        let kind = raw["type"]
            .as_str()
            .context("rollout record type missing")?;
        ensure!(kind != "session_meta", "unexpected own-source metadata");
        // Response items carry native metadata, including calls and outputs.
        // Do not search nested compacted replacements for identities.
        // https://github.com/openai/codex/blob/3d2ee51ca2d5db578f328aa75e20aa22c0197c9a/codex-rs/protocol/src/models.rs
        let addressed = match kind {
            "turn_context" | "token_usage_record" | "event_msg" => payload.get("turn_id"),
            "response_item" => {
                payload.pointer("/internal_chat_message_metadata_passthrough/turn_id")
            }
            _ => None,
        };
        let Some(id) = addressed.filter(|value| !value.is_null()) else {
            return self.unassigned(reference.range, bytes);
        };
        let id = id.as_str().context("invalid native turn id")?;
        identifier(id)?;
        if let Some(thread) = payload.get("thread_id").filter(|value| !value.is_null()) {
            ensure!(
                thread.as_str() == Some(&self.node.native_id),
                "foreign native turn record"
            );
        }
        ensure!(
            self.runs.contains_key(id) || self.runs.len() < MAX_GRAPH_ITEMS,
            "rollout turn limit"
        );
        // Reserve all possible duplicate references and bounded identity fields
        // before growing a run. Raw payloads themselves remain in the store.
        reserve(
            bytes,
            serde_json::to_vec(&reference)?.len() * 4 + id.len() + 128,
        )?;
        let root = if matches!(kind, "turn_context" | "token_usage_record") {
            payload
                .get("root_turn_id")
                .filter(|value| !value.is_null())
                .map(|value| {
                    let id = value.as_str().context("invalid root turn id")?;
                    identifier(id)?;
                    reserve(bytes, id.len() * 2)?;
                    Ok::<_, anyhow::Error>(id.to_owned())
                })
                .transpose()?
        } else {
            None
        };
        let run = self.runs.entry(id.into()).or_insert_with(|| RolloutRun {
            turn_id: id.into(),
            ..Default::default()
        });
        run.records.push(reference.clone());
        if let Some(root) = root.as_ref() {
            if run.root_turn_id.as_ref().is_some_and(|old| old != root) {
                run.gaps.insert("native_rollout_root_turn_conflict".into());
            } else {
                run.root_turn_id = Some(root.clone());
            }
        }
        if kind == "turn_context" {
            run.contexts.push(TurnContext {
                record: reference,
                root_turn_id: root,
            });
        } else if kind == "event_msg" {
            match payload["type"].as_str() {
                Some("task_started" | "turn_started") => run.starts.push(reference),
                Some("task_complete" | "turn_complete") => run.terminations.push(Termination {
                    record: reference,
                    outcome: if payload.get("error").is_some_and(|value| !value.is_null()) {
                        RunOutcome::Failed
                    } else {
                        RunOutcome::Completed
                    },
                }),
                Some("turn_aborted") => {
                    ensure!(
                        matches!(
                            payload["reason"].as_str(),
                            Some("interrupted" | "replaced" | "review_ended" | "budget_limited")
                        ),
                        "unknown abort reason"
                    );
                    run.terminations.push(Termination {
                        record: reference,
                        outcome: RunOutcome::Interrupted,
                    });
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn unassigned(&mut self, range: SourceRange, bytes: &mut usize) -> Result<()> {
        if let Some(previous) = self.evidence.unassigned.last_mut()
            && previous.end == range.start
        {
            previous.end = range.end;
        } else {
            reserve(bytes, serde_json::to_vec(&range)?.len())?;
            self.evidence.unassigned.push(range);
        }
        Ok(())
    }

    pub fn finish(mut self) -> RolloutExecutions {
        for run in self.runs.values_mut() {
            run.gaps.extend(self.evidence.gaps.iter().cloned());
            if run.gaps.contains("native_rollout_root_turn_conflict") {
                run.root_turn_id = None;
            }
            if run.starts.len() != 1 || run.terminations.len() != 1 {
                run.gaps.insert("native_rollout_bookends_incomplete".into());
                continue;
            }
            let start = &run.starts[0];
            let end = &run.terminations[0];
            if start.range.start >= end.record.range.start
                || run.records.iter().any(|record| {
                    record.range.start < start.range.start
                        || record.range.end > end.record.range.end
                })
            {
                run.gaps.insert("native_rollout_bookends_invalid".into());
            }
            if run.gaps.is_empty() {
                run.observed_span = Some(SourceRange {
                    end: end.record.range.end,
                    ..start.range.clone()
                });
                run.outcome = Some(end.outcome);
            }
        }
        // Concurrent or reused starts cannot provide disjoint own executions.
        let spans: Vec<_> = self
            .runs
            .iter()
            .filter_map(|(id, run)| {
                let first = run.starts.first()?;
                let end = run
                    .terminations
                    .last()
                    .filter(|end| {
                        run.starts
                            .last()
                            .is_some_and(|start| start.range.start < end.record.range.start)
                    })
                    .map(|end| end.record.range.end)
                    .or_else(|| self.evidence.inspected.as_ref().map(|range| range.end))?;
                Some((
                    id.clone(),
                    SourceRange {
                        end,
                        ..first.range.clone()
                    },
                ))
            })
            .collect();
        let mut overlaps = BTreeSet::new();
        for (index, (id, span)) in spans.iter().enumerate() {
            for (other, candidate) in &spans[index + 1..] {
                if span.start < candidate.end && candidate.start < span.end {
                    overlaps.insert(id.clone());
                    overlaps.insert(other.clone());
                }
            }
        }
        for id in overlaps {
            if let Some(run) = self.runs.get_mut(&id) {
                run.gaps.insert("native_rollout_execution_overlap".into());
                run.observed_span = None;
                run.outcome = None;
            }
        }
        self.evidence.runs = self.runs.into_values().collect();
        self.evidence
    }
}

fn reserve(bytes: &mut usize, size: usize) -> Result<()> {
    ensure!(size <= *bytes, "rollout execution detail limit");
    *bytes -= size;
    Ok(())
}

#[cfg(test)]
mod tests;
