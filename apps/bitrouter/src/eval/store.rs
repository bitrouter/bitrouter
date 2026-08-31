//! Append-only persistence for generic evaluation exchange records.

use std::collections::{BTreeMap, BTreeSet};

use anyhow::{Context, Result};
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, Set,
    TransactionTrait,
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
        pub owner_user_id: String,
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
        pub owner_user_id: String,
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
        pub owner_user_id: String,
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
    pub subject_content_digest: String,
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
        self.insert_subject_owned(subject, "local").await
    }

    pub async fn insert_subject_owned(
        &self,
        subject: &EvalSubject,
        owner_user_id: &str,
    ) -> Result<SubjectInsertOutcome> {
        validate_owner(owner_user_id)?;
        validate_subject(subject)?;
        let content_digest = subject.semantic_digest()?;
        if let Some(existing) = subject_entity::Entity::find_by_id(&subject.eval_id)
            .one(&self.db)
            .await?
        {
            if existing.owner_user_id == owner_user_id && existing.content_digest == content_digest
            {
                return Ok(SubjectInsertOutcome::Duplicate);
            }
            anyhow::bail!(
                "eval subject '{}' already exists with different content",
                subject.eval_id
            );
        }
        subject_entity::ActiveModel {
            eval_id: Set(subject.eval_id.clone()),
            owner_user_id: Set(owner_user_id.to_string()),
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
        row.map(stored_subject).transpose()
    }

    pub async fn subject_for_owner(
        &self,
        eval_id: &str,
        owner_user_id: &str,
    ) -> Result<Option<EvalSubject>> {
        subject_entity::Entity::find_by_id(eval_id)
            .filter(subject_entity::Column::OwnerUserId.eq(owner_user_id))
            .one(&self.db)
            .await?
            .map(stored_subject)
            .transpose()
    }

    pub async fn list_subjects(&self) -> Result<Vec<EvalSubject>> {
        subject_entity::Entity::find()
            .order_by_asc(subject_entity::Column::EvalId)
            .all(&self.db)
            .await?
            .into_iter()
            .map(stored_subject)
            .collect()
    }

    pub async fn list_subjects_for_owner(&self, owner_user_id: &str) -> Result<Vec<EvalSubject>> {
        subject_entity::Entity::find()
            .filter(subject_entity::Column::OwnerUserId.eq(owner_user_id))
            .order_by_asc(subject_entity::Column::EvalId)
            .all(&self.db)
            .await?
            .into_iter()
            .map(stored_subject)
            .collect()
    }

    pub async fn insert_result(&self, result: &EvaluationResult) -> Result<ResultInsertOutcome> {
        self.insert_result_owned(result, "local").await
    }

    pub async fn insert_result_owned(
        &self,
        result: &EvaluationResult,
        owner_user_id: &str,
    ) -> Result<ResultInsertOutcome> {
        validate_owner(owner_user_id)?;
        validate_result(result)?;
        if self
            .subject_for_owner(&result.eval_id, owner_user_id)
            .await?
            .is_none()
        {
            anyhow::bail!("unknown eval subject '{}'", result.eval_id);
        }
        let content_digest = result.semantic_digest()?;
        if let Some(existing) = result_entity::Entity::find()
            .filter(result_entity::Column::OwnerUserId.eq(owner_user_id))
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
            owner_user_id: Set(owner_user_id.to_string()),
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

    pub async fn latest_admissions_for_owner(
        &self,
        owner_user_id: &str,
    ) -> Result<BTreeMap<String, AdmissionEvent>> {
        let owned_ids = result_entity::Entity::find()
            .filter(result_entity::Column::OwnerUserId.eq(owner_user_id))
            .all(&self.db)
            .await?
            .into_iter()
            .map(|row| row.result_id)
            .collect::<std::collections::BTreeSet<_>>();
        Ok(self
            .latest_admissions()
            .await?
            .into_iter()
            .filter(|(result_id, _)| owned_ids.contains(result_id))
            .collect())
    }

    pub async fn freeze_snapshot(&self, frozen_at: &str) -> Result<EvalSnapshot> {
        let snapshot = self.materialize_snapshot(frozen_at).await?;
        self.persist_snapshot_scoped(&snapshot, None).await
    }

    /// Materialize the current admitted result set as one immutable manifest
    /// inside a consistent read transaction without persisting a snapshot row.
    pub async fn materialize_snapshot(&self, frozen_at: &str) -> Result<EvalSnapshot> {
        self.materialize_snapshot_scoped(frozen_at, None, None)
            .await
    }

    /// Persist an exact manifest previously returned by
    /// [`Self::materialize_snapshot`].
    pub async fn persist_snapshot(&self, snapshot: &EvalSnapshot) -> Result<EvalSnapshot> {
        self.persist_snapshot_scoped(snapshot, None).await
    }

    pub async fn freeze_snapshot_for_owner(
        &self,
        frozen_at: &str,
        owner_user_id: &str,
    ) -> Result<EvalSnapshot> {
        validate_owner(owner_user_id)?;
        let snapshot = self
            .materialize_snapshot_scoped(frozen_at, Some(owner_user_id), None)
            .await?;
        self.persist_snapshot_scoped(&snapshot, Some(owner_user_id))
            .await
    }

    /// Freeze exactly the named admitted results for one owner. This is the
    /// controlled-experiment path: unrelated historical evidence must not be
    /// able to enter the candidate compiler through a process-wide snapshot.
    pub async fn freeze_snapshot_for_result_ids(
        &self,
        frozen_at: &str,
        owner_user_id: &str,
        result_ids: &[String],
    ) -> Result<EvalSnapshot> {
        validate_owner(owner_user_id)?;
        let selected = result_ids.iter().cloned().collect::<BTreeSet<_>>();
        if selected.is_empty() || selected.len() != result_ids.len() {
            anyhow::bail!("snapshot result ids must be a non-empty unique set");
        }
        let snapshot = self
            .materialize_snapshot_scoped(frozen_at, Some(owner_user_id), Some(&selected))
            .await?;
        self.persist_snapshot_scoped(&snapshot, Some(owner_user_id))
            .await
    }

    async fn materialize_snapshot_scoped(
        &self,
        frozen_at: &str,
        owner_user_id: Option<&str>,
        selected_result_ids: Option<&BTreeSet<String>>,
    ) -> Result<EvalSnapshot> {
        chrono::DateTime::parse_from_rfc3339(frozen_at)
            .context("snapshot frozen_at must be RFC3339")?;
        let transaction = self.db.begin().await?;
        let admission_rows = admission_entity::Entity::find()
            .order_by_asc(admission_entity::Column::Sequence)
            .all(&transaction)
            .await?;
        let mut latest = BTreeMap::new();
        for row in admission_rows {
            let event = admission_event(row)?;
            latest.insert(event.result_id.clone(), event);
        }
        let mut query = result_entity::Entity::find();
        if let Some(owner_user_id) = owner_user_id {
            query = query.filter(result_entity::Column::OwnerUserId.eq(owner_user_id));
        }
        if let Some(result_ids) = selected_result_ids {
            query = query.filter(result_entity::Column::ResultId.is_in(result_ids.iter().cloned()));
        }
        let rows = query
            .order_by_asc(result_entity::Column::ResultId)
            .all(&transaction)
            .await?;
        let mut entries = Vec::new();
        for row in rows {
            let admitted = latest
                .get(&row.result_id)
                .is_some_and(|event| event.status == AdmissionStatus::Admitted);
            if !admitted && selected_result_ids.is_some() {
                anyhow::bail!("selected eval result '{}' is not admitted", row.result_id);
            }
            if !admitted {
                continue;
            }
            let subject = subject_entity::Entity::find_by_id(&row.eval_id)
                .one(&transaction)
                .await?
                .ok_or_else(|| {
                    anyhow::anyhow!(
                        "admitted result '{}' references missing subject '{}'",
                        row.result_id,
                        row.eval_id
                    )
                })?;
            if subject.owner_user_id != row.owner_user_id {
                anyhow::bail!(
                    "eval result '{}' and subject '{}' have different owners",
                    row.result_id,
                    row.eval_id
                );
            }
            entries.push(EvalSnapshotEntry {
                result_id: row.result_id,
                content_digest: row.content_digest,
                subject_content_digest: subject.content_digest,
                eval_id: row.eval_id,
            });
        }
        if let Some(result_ids) = selected_result_ids
            && entries.len() != result_ids.len()
        {
            anyhow::bail!("one or more selected eval results do not exist for this owner");
        }
        let snapshot_owner = owner_user_id.unwrap_or("*");
        let evidence_root = canonical_digest(&(snapshot_owner, frozen_at, &entries))?;
        let snapshot = EvalSnapshot {
            evidence_root: evidence_root.clone(),
            frozen_at: frozen_at.to_string(),
            entries,
        };
        transaction.commit().await?;
        Ok(snapshot)
    }

    async fn persist_snapshot_scoped(
        &self,
        snapshot: &EvalSnapshot,
        owner_user_id: Option<&str>,
    ) -> Result<EvalSnapshot> {
        chrono::DateTime::parse_from_rfc3339(&snapshot.frozen_at)
            .context("snapshot frozen_at must be RFC3339")?;
        let snapshot_owner = owner_user_id.unwrap_or("*");
        let expected_root = canonical_digest(&(
            snapshot_owner,
            snapshot.frozen_at.as_str(),
            &snapshot.entries,
        ))?;
        if expected_root != snapshot.evidence_root {
            anyhow::bail!("eval snapshot manifest does not match its evidence root");
        }
        if let Some(existing) = self.snapshot_by_root(&snapshot.evidence_root).await? {
            if existing == *snapshot {
                return Ok(existing);
            }
            anyhow::bail!("eval snapshot root already exists with different entries");
        }
        let manifest_json = serde_json::to_string(&snapshot)?;
        let result_count = i32::try_from(snapshot.entries.len())
            .context("eval snapshot contains too many rows")?;
        snapshot_entity::ActiveModel {
            evidence_root: Set(snapshot.evidence_root.clone()),
            owner_user_id: Set(snapshot_owner.to_string()),
            manifest_json: Set(manifest_json),
            result_count: Set(result_count),
            frozen_at: Set(snapshot.frozen_at.clone()),
        }
        .insert(&self.db)
        .await
        .context("freezing eval snapshot")?;
        Ok(snapshot.clone())
    }

    pub async fn snapshot_by_root(&self, evidence_root: &str) -> Result<Option<EvalSnapshot>> {
        snapshot_entity::Entity::find_by_id(evidence_root)
            .one(&self.db)
            .await?
            .map(stored_snapshot)
            .transpose()
    }

    pub async fn snapshot_by_root_for_owner(
        &self,
        evidence_root: &str,
        owner_user_id: &str,
    ) -> Result<Option<EvalSnapshot>> {
        snapshot_entity::Entity::find_by_id(evidence_root)
            .filter(snapshot_entity::Column::OwnerUserId.eq(owner_user_id))
            .one(&self.db)
            .await?
            .map(stored_snapshot)
            .transpose()
    }
}

fn validate_owner(owner_user_id: &str) -> Result<()> {
    if owner_user_id.trim().is_empty() || owner_user_id.len() > 512 {
        anyhow::bail!("eval owner user id must contain 1 to 512 characters")
    }
    Ok(())
}

fn stored_subject(row: subject_entity::Model) -> Result<EvalSubject> {
    let subject: EvalSubject =
        serde_json::from_str(&row.subject_json).context("parsing stored eval subject")?;
    if subject.eval_id != row.eval_id || subject.semantic_digest()? != row.content_digest {
        anyhow::bail!("stored eval subject does not match its content address");
    }
    Ok(subject)
}

fn stored_result(row: result_entity::Model) -> Result<StoredEvaluationResult> {
    let result: EvaluationResult =
        serde_json::from_str(&row.result_json).context("parsing stored evaluation result")?;
    let semantic_digest = result.semantic_digest()?;
    if result.eval_id != row.eval_id
        || semantic_digest != row.content_digest
        || semantic_digest != row.result_id
    {
        anyhow::bail!("stored evaluation result does not match its content address");
    }
    Ok(StoredEvaluationResult {
        result_id: row.result_id,
        content_digest: row.content_digest,
        result,
    })
}

fn stored_snapshot(row: snapshot_entity::Model) -> Result<EvalSnapshot> {
    let snapshot: EvalSnapshot =
        serde_json::from_str(&row.manifest_json).context("parsing eval snapshot")?;
    if snapshot.evidence_root != row.evidence_root
        || snapshot.frozen_at != row.frozen_at
        || i32::try_from(snapshot.entries.len()).ok() != Some(row.result_count)
    {
        anyhow::bail!("stored eval snapshot does not match its indexed metadata");
    }
    let actual_root = canonical_digest(&(
        row.owner_user_id.as_str(),
        snapshot.frozen_at.as_str(),
        &snapshot.entries,
    ))?;
    if actual_root != snapshot.evidence_root {
        anyhow::bail!("stored eval snapshot manifest does not match its evidence root");
    }
    Ok(snapshot)
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

    use sea_orm::IntoActiveModel;

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
        store.insert_subject(&subject()?).await?;
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

    #[tokio::test]
    async fn snapshot_root_commits_subject_decisions() -> anyhow::Result<()> {
        async fn root_for(selected_tier: &str) -> anyhow::Result<String> {
            let store = store().await?;
            let mut subject = subject()?;
            subject.decisions = vec![EvalDecisionRef {
                decision_id: "decision-1".into(),
                policy: "auto".into(),
                route_projection: "agent_route/v1|unknown|implement|normal".into(),
                request_key: "agent_route/v1|unknown|implement|normal".into(),
                selected_tier: selected_tier.into(),
                selected_effort: None,
                baseline_tier: Some("strong".into()),
                baseline_effort: None,
                policy_digest: subject.policy_digest.clone(),
                experiment: None,
            }];
            store.insert_subject(&subject).await?;
            let inserted = store.insert_result(&result(EvalVerdict::Pass)).await?;
            store
                .append_admission_event(
                    inserted.result_id(),
                    AdmissionStatus::Admitted,
                    "admitted",
                    "local",
                )
                .await?;
            Ok(store
                .freeze_snapshot("2026-07-30T00:02:00Z")
                .await?
                .evidence_root)
        }

        assert_ne!(root_for("economy").await?, root_for("strong").await?);
        Ok(())
    }

    #[tokio::test]
    async fn selected_snapshot_excludes_other_admitted_local_results() -> anyhow::Result<()> {
        let store = store().await?;
        let historical_subject = subject()?;
        store.insert_subject(&historical_subject).await?;
        let historical = store.insert_result(&result(EvalVerdict::Pass)).await?;
        store
            .append_admission_event(
                historical.result_id(),
                AdmissionStatus::Admitted,
                "admitted",
                "local",
            )
            .await?;

        let mut current_subject = subject()?;
        current_subject.eval_id = "eval-current".into();
        current_subject.subject_id = "request-current".into();
        store.insert_subject(&current_subject).await?;
        let mut current_result = result(EvalVerdict::Pass);
        current_result.eval_id = current_subject.eval_id.clone();
        current_result.idempotency_key = "submission-current".into();
        let current = store.insert_result(&current_result).await?;
        store
            .append_admission_event(
                current.result_id(),
                AdmissionStatus::Admitted,
                "admitted",
                "local",
            )
            .await?;

        let snapshot = store
            .freeze_snapshot_for_result_ids(
                "2026-07-30T00:02:00Z",
                "local",
                &[current.result_id().to_string()],
            )
            .await?;

        assert_eq!(snapshot.entries.len(), 1);
        assert_eq!(snapshot.entries[0].result_id, current.result_id());
        assert!(
            store
                .freeze_snapshot_for_result_ids(
                    "2026-07-30T00:03:00Z",
                    "local",
                    &["sha256:missing".into()],
                )
                .await
                .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn snapshot_manifest_tampering_breaks_content_address_verification() -> anyhow::Result<()>
    {
        let store = store().await?;
        store.insert_subject(&subject()?).await?;
        let inserted = store.insert_result(&result(EvalVerdict::Pass)).await?;
        store
            .append_admission_event(
                inserted.result_id(),
                AdmissionStatus::Admitted,
                "admitted",
                "local",
            )
            .await?;
        let snapshot = store.freeze_snapshot("2026-07-30T00:02:00Z").await?;
        let row = snapshot_entity::Entity::find_by_id(&snapshot.evidence_root)
            .one(&store.db)
            .await?
            .ok_or_else(|| anyhow::anyhow!("snapshot row is missing"))?;
        let mut tampered = snapshot.clone();
        tampered.entries[0].eval_id = "different-eval".into();
        let mut active = row.into_active_model();
        active.manifest_json = Set(serde_json::to_string(&tampered)?);
        active.update(&store.db).await?;

        assert!(
            store
                .snapshot_by_root(&snapshot.evidence_root)
                .await
                .is_err()
        );
        Ok(())
    }
}
