//! Frozen evidence export preserves exact subject identity; it never compiles Policy.

#[cfg(feature = "core")]
use std::collections::BTreeSet;

use anyhow::Result;
use serde::Serialize;

use bitrouter_ai::eval::{EvalSubject, EvaluationResult};
use {
    super::store::{EvalStore, SnapshotRecords},
    bitrouter_ai::eval::EvalSnapshot,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvalEvidenceRecord {
    pub result_id: String,
    pub content_digest: String,
    pub subject: EvalSubject,
    pub result: EvaluationResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct EvalEvidenceSnapshot {
    pub evidence_root: String,
    pub frozen_at: String,
    pub records: Vec<EvalEvidenceRecord>,
}

impl EvalEvidenceSnapshot {
    pub async fn load(store: &EvalStore, evidence_root: &str) -> Result<Self> {
        let stored = store
            .snapshot_records(evidence_root)
            .await?
            .ok_or_else(|| anyhow::anyhow!("eval snapshot '{evidence_root}' not found"))?;
        Self::from_records(stored)
    }

    /// Export evidence from an in-memory immutable manifest without aggregation.
    pub async fn from_manifest(store: &EvalStore, snapshot: EvalSnapshot) -> Result<Self> {
        // The store validates tenant roots and the whole immutable closure in
        // one stable transaction with a cumulative row/byte budget.
        let stored_records = store.manifest_records(&snapshot).await?;
        Self::from_records(SnapshotRecords {
            snapshot,
            records: stored_records,
        })
    }

    pub(crate) fn from_records(stored: SnapshotRecords) -> Result<Self> {
        let SnapshotRecords {
            snapshot,
            records: stored_records,
        } = stored;
        let mut records = Vec::with_capacity(stored_records.len());
        for (stored, subject) in stored_records {
            crate::storage::evaluation::validate_result_for_subject(&stored.result, &subject)?;
            records.push(EvalEvidenceRecord {
                result_id: stored.result_id,
                content_digest: stored.content_digest,
                subject,
                result: stored.result,
            });
        }
        records.sort_by(|left, right| left.result_id.cmp(&right.result_id));
        Ok(Self {
            evidence_root: snapshot.evidence_root,
            frozen_at: snapshot.frozen_at,
            records,
        })
    }
}

#[cfg(feature = "core")]
pub(crate) fn classifier_cohorts(subject: &EvalSubject) -> BTreeSet<String> {
    let mut cohorts = subject
        .evidence
        .iter()
        .filter(|item| item.kind == "routing.assessment")
        .filter_map(|item| item.attributes.get("classifier_digest").cloned())
        .collect::<BTreeSet<_>>();
    if cohorts.is_empty() {
        cohorts.insert("unassessed".into());
    }
    cohorts
}
