//! Append-only persistence for generic evaluation exchange records.

use std::collections::BTreeMap;

use anyhow::{Context, Result};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
};
use serde::{Deserialize, Serialize};

use super::types::{
    AdmissionStatus, EvalScope, EvalSubject, EvaluationResult, canonical_digest, validate_result,
    validate_subject,
};

mod subject_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_subjects")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub eval_id: String,
        pub subject_id: String,
        pub scope: String,
        pub policy_digest: String,
        pub holdout: bool,
        pub content_digest: String,
        pub subject_json: String,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod result_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_results")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub result_id: String,
        pub eval_id: String,
        pub idempotency_key: String,
        pub authority_id: String,
        pub evaluator_kind: String,
        pub content_digest: String,
        pub result_json: String,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod admission_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_admission_events")]
    pub struct Model {
        #[sea_orm(primary_key)]
        pub sequence: i64,
        pub result_id: String,
        pub status: String,
        pub reason: String,
        pub authority_id: String,
        pub created_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

mod snapshot_entity {
    use sea_orm::entity::prelude::*;

    #[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
    #[sea_orm(table_name = "eval_snapshots")]
    pub struct Model {
        #[sea_orm(primary_key, auto_increment = false)]
        pub evidence_root: String,
        pub manifest_json: String,
        pub result_count: i32,
        pub frozen_at: String,
    }

    #[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
    pub enum Relation {}
    impl ActiveModelBehavior for ActiveModel {}
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SubjectInsertOutcome {
    Inserted,
    Duplicate,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ResultInsertOutcome {
    Inserted { result_id: String },
    Duplicate { result_id: String },
}

impl ResultInsertOutcome {
    pub fn result_id(&self) -> &str {
        match self {
            Self::Inserted { result_id } | Self::Duplicate { result_id } => result_id,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AdmissionEvent {
    pub sequence: i64,
    pub result_id: String,
    pub status: AdmissionStatus,
    pub reason: String,
    pub authority_id: String,
    pub created_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct StoredEvaluationResult {
    pub result_id: String,
    pub content_digest: String,
    pub result: EvaluationResult,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSnapshotEntry {
    pub result_id: String,
    pub content_digest: String,
    pub eval_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvalSnapshot {
    pub evidence_root: String,
    pub frozen_at: String,
    pub entries: Vec<EvalSnapshotEntry>,
}

#[derive(Clone)]
pub struct EvalStore {
    db: DatabaseConnection,
}

impl EvalStore {
    pub fn new(db: DatabaseConnection) -> Self {
        Self { db }
    }

    pub async fn insert_subject(&self, subject: &EvalSubject) -> Result<SubjectInsertOutcome> {
        validate_subject(subject)?;
        let content_digest = subject.semantic_digest()?;
        if let Some(existing) = subject_entity::Entity::find_by_id(&subject.eval_id)
            .one(&self.db)
            .await?
        {
            if existing.content_digest == content_digest {
                return Ok(SubjectInsertOutcome::Duplicate);
            }
            anyhow::bail!(
                "eval subject '{}' already exists with different content",
                subject.eval_id
            );
        }
        subject_entity::ActiveModel {
            eval_id: Set(subject.eval_id.clone()),
            subject_id: Set(subject.subject_id.clone()),
            scope: Set(scope_name(subject.scope).into()),
            policy_digest: Set(subject.policy_digest.clone()),
            holdout: Set(subject.holdout),
            content_digest: Set(content_digest),
            subject_json: Set(serde_json::to_string(subject)?),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(&self.db)
        .await
        .context("inserting eval subject")?;
        Ok(SubjectInsertOutcome::Inserted)
    }

    pub async fn subject(&self, eval_id: &str) -> Result<Option<EvalSubject>> {
        let row = subject_entity::Entity::find_by_id(eval_id)
            .one(&self.db)
            .await?;
        row.map(|row| {
            serde_json::from_str(&row.subject_json).context("parsing stored eval subject")
        })
        .transpose()
    }

    pub async fn list_subjects(&self) -> Result<Vec<EvalSubject>> {
        subject_entity::Entity::find()
            .order_by_asc(subject_entity::Column::EvalId)
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| {
                serde_json::from_str(&row.subject_json).context("parsing stored eval subject")
            })
            .collect()
    }

    pub async fn insert_result(&self, result: &EvaluationResult) -> Result<ResultInsertOutcome> {
        validate_result(result)?;
        let content_digest = result.semantic_digest()?;
        if let Some(existing) = result_entity::Entity::find()
            .filter(result_entity::Column::IdempotencyKey.eq(&result.idempotency_key))
            .one(&self.db)
            .await?
        {
            if existing.content_digest == content_digest {
                return Ok(ResultInsertOutcome::Duplicate {
                    result_id: existing.result_id,
                });
            }
            anyhow::bail!(
                "idempotency key '{}' already exists with different content",
                result.idempotency_key
            );
        }
        let result_id = content_digest.clone();
        result_entity::ActiveModel {
            result_id: Set(result_id.clone()),
            eval_id: Set(result.eval_id.clone()),
            idempotency_key: Set(result.idempotency_key.clone()),
            authority_id: Set(result.evaluator.authority_id.clone()),
            evaluator_kind: Set(format!("{:?}", result.evaluator.kind).to_ascii_lowercase()),
            content_digest: Set(content_digest),
            result_json: Set(serde_json::to_string(result)?),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
        }
        .insert(&self.db)
        .await
        .context("inserting evaluation result")?;
        Ok(ResultInsertOutcome::Inserted { result_id })
    }

    pub async fn result(&self, result_id: &str) -> Result<Option<StoredEvaluationResult>> {
        result_entity::Entity::find_by_id(result_id)
            .one(&self.db)
            .await?
            .map(stored_result)
            .transpose()
    }

    pub async fn results_for_subject(&self, eval_id: &str) -> Result<Vec<StoredEvaluationResult>> {
        result_entity::Entity::find()
            .filter(result_entity::Column::EvalId.eq(eval_id))
            .order_by_asc(result_entity::Column::ResultId)
            .all(&self.db)
            .await?
            .into_iter()
            .map(stored_result)
            .collect()
    }

    pub async fn append_admission_event(
        &self,
        result_id: &str,
        status: AdmissionStatus,
        reason: &str,
        authority_id: &str,
    ) -> Result<AdmissionEvent> {
        if self.result(result_id).await?.is_none() {
            anyhow::bail!("cannot admit unknown result '{result_id}'");
        }
        let created_at = chrono::Utc::now().to_rfc3339();
        let inserted = admission_entity::ActiveModel {
            sequence: Default::default(),
            result_id: Set(result_id.to_string()),
            status: Set(admission_status_name(status).into()),
            reason: Set(reason.to_string()),
            authority_id: Set(authority_id.to_string()),
            created_at: Set(created_at.clone()),
        }
        .insert(&self.db)
        .await
        .context("appending eval admission event")?;
        Ok(AdmissionEvent {
            sequence: inserted.sequence,
            result_id: inserted.result_id,
            status,
            reason: inserted.reason,
            authority_id: inserted.authority_id,
            created_at,
        })
    }

    pub async fn latest_admissions(&self) -> Result<BTreeMap<String, AdmissionEvent>> {
        let rows = admission_entity::Entity::find()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(&self.db)
            .await?;
        let mut latest = BTreeMap::new();
        for row in rows {
            let event = admission_event(row)?;
            latest.insert(event.result_id.clone(), event);
        }
        Ok(latest)
    }

    pub async fn freeze_snapshot(&self, frozen_at: &str) -> Result<EvalSnapshot> {
        chrono::DateTime::parse_from_rfc3339(frozen_at)
            .context("snapshot frozen_at must be RFC3339")?;
        let latest = self.latest_admissions().await?;
        let rows = result_entity::Entity::find()
            .order_by_asc(result_entity::Column::ResultId)
            .all(&self.db)
            .await?;
        let entries = rows
            .into_iter()
            .filter(|row| {
                latest
                    .get(&row.result_id)
                    .is_some_and(|event| event.status == AdmissionStatus::Admitted)
            })
            .map(|row| EvalSnapshotEntry {
                result_id: row.result_id,
                content_digest: row.content_digest,
                eval_id: row.eval_id,
            })
            .collect::<Vec<_>>();
        let evidence_root = canonical_digest(&entries)?;
        let snapshot = EvalSnapshot {
            evidence_root: evidence_root.clone(),
            frozen_at: frozen_at.to_string(),
            entries,
        };
        if let Some(existing) = self.snapshot_by_root(&snapshot.evidence_root).await? {
            if existing.entries == snapshot.entries {
                return Ok(existing);
            }
            anyhow::bail!("eval snapshot root already exists with different entries");
        }
        let manifest_json = serde_json::to_string(&snapshot)?;
        let result_count = match i32::try_from(snapshot.entries.len()) {
            Ok(value) => value,
            Err(_) => i32::MAX,
        };
        snapshot_entity::ActiveModel {
            evidence_root: Set(evidence_root),
            manifest_json: Set(manifest_json),
            result_count: Set(result_count),
            frozen_at: Set(frozen_at.to_string()),
        }
        .insert(&self.db)
        .await
        .context("freezing eval snapshot")?;
        Ok(snapshot)
    }

    pub async fn snapshot_by_root(&self, evidence_root: &str) -> Result<Option<EvalSnapshot>> {
        snapshot_entity::Entity::find_by_id(evidence_root)
            .one(&self.db)
            .await?
            .map(|row| serde_json::from_str(&row.manifest_json).context("parsing eval snapshot"))
            .transpose()
    }
}

fn stored_result(row: result_entity::Model) -> Result<StoredEvaluationResult> {
    Ok(StoredEvaluationResult {
        result_id: row.result_id,
        content_digest: row.content_digest,
        result: serde_json::from_str(&row.result_json)
            .context("parsing stored evaluation result")?,
    })
}

fn admission_event(row: admission_entity::Model) -> Result<AdmissionEvent> {
    Ok(AdmissionEvent {
        sequence: row.sequence,
        result_id: row.result_id,
        status: parse_admission_status(&row.status)?,
        reason: row.reason,
        authority_id: row.authority_id,
        created_at: row.created_at,
    })
}

fn scope_name(scope: EvalScope) -> &'static str {
    match scope {
        EvalScope::Request => "request",
        EvalScope::Episode => "episode",
        EvalScope::Task => "task",
    }
}

fn admission_status_name(status: AdmissionStatus) -> &'static str {
    match status {
        AdmissionStatus::Admitted => "admitted",
        AdmissionStatus::Rejected => "rejected",
        AdmissionStatus::HeldOut => "held_out",
        AdmissionStatus::Disputed => "disputed",
    }
}

fn parse_admission_status(value: &str) -> Result<AdmissionStatus> {
    match value {
        "admitted" => Ok(AdmissionStatus::Admitted),
        "rejected" => Ok(AdmissionStatus::Rejected),
        "held_out" => Ok(AdmissionStatus::HeldOut),
        "disputed" => Ok(AdmissionStatus::Disputed),
        _ => anyhow::bail!("unknown stored admission status '{value}'"),
    }
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use super::*;
    use crate::eval::types::*;

    async fn store() -> anyhow::Result<EvalStore> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        Ok(EvalStore::new(db))
    }

    fn subject() -> anyhow::Result<EvalSubject> {
        let evidence = Vec::new();
        Ok(EvalSubject {
            schema_version: 1,
            eval_id: "eval-1".into(),
            scope: EvalScope::Request,
            subject_id: "request-1".into(),
            policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            preset: Some("auto".into()),
            cohort: None,
            holdout: false,
            decisions: Vec::new(),
            requested_dimensions: BTreeSet::new(),
            evidence_digest: evidence_digest(&evidence)?,
            evidence,
            observed_at: "2026-07-30T00:00:00Z".into(),
        })
    }

    fn result(verdict: EvalVerdict) -> EvaluationResult {
        EvaluationResult {
            schema_version: 1,
            eval_id: "eval-1".into(),
            evidence_digest:
                "sha256:4f53cda18c2baa0c0354bb5f9a3ecbe5ed12ab4d8e11ba873c2f11161202b945".into(),
            evaluator: EvaluatorIdentity {
                authority_id: "local".into(),
                evaluator_id: "human".into(),
                kind: EvaluatorKind::Human,
                version: "1".into(),
                config_digest:
                    "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            },
            verdict,
            metrics: BTreeMap::new(),
            hard_violations: Vec::new(),
            confidence_ppm: None,
            evidence_refs: Vec::new(),
            decision_credit: BTreeMap::new(),
            idempotency_key: "submission-1".into(),
            submitted_at: "2026-07-30T00:01:00Z".into(),
        }
    }

    #[tokio::test]
    async fn subject_and_result_round_trip() -> anyhow::Result<()> {
        let store = store().await?;
        let subject = subject()?;
        store.insert_subject(&subject).await?;
        store.insert_result(&result(EvalVerdict::Pass)).await?;

        assert_eq!(store.subject("eval-1").await?, Some(subject));
        assert_eq!(store.results_for_subject("eval-1").await?.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn idempotency_replays_identical_content_and_rejects_conflicts() -> anyhow::Result<()> {
        let store = store().await?;
        let pass = result(EvalVerdict::Pass);
        assert!(matches!(
            store.insert_result(&pass).await?,
            ResultInsertOutcome::Inserted { .. }
        ));
        assert!(matches!(
            store.insert_result(&pass).await?,
            ResultInsertOutcome::Duplicate { .. }
        ));
        assert!(
            store
                .insert_result(&result(EvalVerdict::Fail))
                .await
                .is_err()
        );
        Ok(())
    }
}
