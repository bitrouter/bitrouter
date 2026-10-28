//! Append-only persistence for generic evaluation exchange records.

use bitrouter_ai::eval::{EvalSnapshot, EvalSnapshotEntry, SubjectInsertOutcome};
use std::collections::{BTreeMap, BTreeSet};

use crate::storage::scope::{KeyDomain, MetadataRef, Provenance, Scope};
use anyhow::{Context, Result, ensure};
#[cfg(test)]
use sea_orm::ActiveModelTrait;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder,
    QuerySelect, QueryTrait, Select, Set,
};
use serde::{Deserialize, Serialize};

mod codec;
mod imported;
mod io;
pub(crate) mod migration;
mod owned;
#[cfg(test)]
mod owned_tests;
mod snapshots;
mod trajectory;
use bitrouter_ai::tenancy::operation::ErrorCode;
use io::Budget;

/// Redacted classification for the private actor-owned ledger boundary.
#[derive(Debug, thiserror::Error)]
pub(crate) enum OwnedError {
    #[error("owned evaluation request rejected")]
    Rejected(ErrorCode),
    #[error("owned evaluation integrity failure")]
    Integrity(#[source] anyhow::Error),
    #[error("owned evaluation storage failure")]
    Storage(#[source] anyhow::Error),
}

impl OwnedError {
    fn classify(error: anyhow::Error) -> Self {
        if let Some(error) = error.downcast_ref::<crate::storage::operation_budget::Error>() {
            return match error {
                crate::storage::operation_budget::Error::Rejected(code) => Self::Rejected(*code),
                crate::storage::operation_budget::Error::Capacity => {
                    Self::Rejected(ErrorCode::CapacityExceeded)
                }
                crate::storage::operation_budget::Error::Unavailable => {
                    Self::Rejected(ErrorCode::AuthorityUnavailable)
                }
            };
        }
        match error.downcast::<Self>() {
            Ok(error) => error,
            Err(error) if error.is::<io::Capacity>() => Self::Rejected(ErrorCode::CapacityExceeded),
            Err(error) if error.is::<sea_orm::DbErr>() => Self::Storage(error),
            Err(error) => Self::Integrity(error),
        }
    }
}

use {
    crate::storage::evaluation::canonical_digest, crate::storage::evaluation::validate_result,
    crate::storage::evaluation::validate_subject, bitrouter_ai::eval::AdmissionStatus,
    bitrouter_ai::eval::EvalScope, bitrouter_ai::eval::EvalSubject,
    bitrouter_ai::eval::EvaluationResult,
};

mod entities;
use entities::{admission_entity, result_entity, snapshot_entity, subject_entity};

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

pub(crate) struct SnapshotRecords {
    pub(crate) snapshot: EvalSnapshot,
    pub(crate) records: Vec<(StoredEvaluationResult, EvalSubject)>,
}

#[derive(Clone)]
pub struct EvalStore {
    db: DatabaseConnection,
    scope: Scope,
    provenance: Provenance,
}

impl EvalStore {
    #[cfg(feature = "harness")]
    pub(crate) fn for_paid_recovery(
        identity: &crate::tenancy::learning::recovery::RecoveryIdentity,
    ) -> Self {
        let (scope, provenance) = Scope::for_paid_recovery(identity);
        Self {
            db: identity.database().clone(),
            scope,
            provenance,
        }
    }
    #[cfg(feature = "harness")]
    pub(crate) fn for_learner(identity: &crate::tenancy::learner::identity::ReadIdentity) -> Self {
        let (scope, provenance) = Scope::for_learner(identity);
        Self {
            db: identity.database().clone(),
            scope,
            provenance,
        }
    }

    pub fn new(db: DatabaseConnection) -> Self {
        Self {
            db,
            scope: Scope::legacy(),
            provenance: Provenance::legacy(),
        }
    }

    pub(crate) fn for_execution(
        db: DatabaseConnection,
        execution: &crate::tenancy::execution::TenantExecution,
    ) -> Self {
        let (scope, provenance) = Scope::for_execution(execution);
        Self {
            db,
            scope,
            provenance,
        }
    }

    pub(crate) fn for_owned(identity: &crate::tenancy::eval::EvalIdentity) -> Self {
        let (scope, provenance) = Scope::for_eval(identity);
        Self {
            db: identity.database().clone(),
            scope,
            provenance,
        }
    }

    #[cfg(feature = "harness")]
    pub(crate) fn for_native_session(
        authority: &crate::tenancy::execution::session::Authority,
    ) -> Self {
        let (scope, provenance) = Scope::for_session(authority);
        Self {
            db: authority.database().clone(),
            scope,
            provenance,
        }
    }

    pub(super) fn legacy_only(&self) -> Result<()> {
        ensure!(
            self.scope.is_legacy(),
            "legacy owner API cannot access a tenant eval store"
        );
        Ok(())
    }

    fn key(&self, domain: KeyDomain, logical: &str) -> Result<String> {
        self.scope.physical_key(domain, logical, &[logical])
    }

    fn owner(&self, legacy: &str) -> Result<String> {
        self.scope.physical_key(KeyDomain::Owner, legacy, &[])
    }

    fn subjects(&self) -> Select<subject_entity::Entity> {
        subject_entity::Entity::find().filter(self.scope.predicate(
            subject_entity::Column::InstallationId,
            subject_entity::Column::TenantId,
        ))
    }
    fn results(&self) -> Select<result_entity::Entity> {
        result_entity::Entity::find().filter(self.scope.predicate(
            result_entity::Column::InstallationId,
            result_entity::Column::TenantId,
        ))
    }
    fn admissions(&self) -> Select<admission_entity::Entity> {
        admission_entity::Entity::find().filter(self.scope.predicate(
            admission_entity::Column::InstallationId,
            admission_entity::Column::TenantId,
        ))
    }
    fn snapshots(&self) -> Select<snapshot_entity::Entity> {
        snapshot_entity::Entity::find().filter(self.scope.predicate(
            snapshot_entity::Column::InstallationId,
            snapshot_entity::Column::TenantId,
        ))
    }

    pub async fn insert_subject(&self, subject: &EvalSubject) -> Result<SubjectInsertOutcome> {
        let owner = self.owner("local")?;
        let tx = self.transaction(true).await?;
        let outcome = self
            .insert_subject_in(&tx, subject, &owner, &mut Budget::new(self))
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn insert_subject_owned(
        &self,
        subject: &EvalSubject,
        owner_user_id: &str,
    ) -> Result<SubjectInsertOutcome> {
        self.legacy_only()?;
        validate_owner(owner_user_id)?;
        let tx = self.transaction(true).await?;
        let outcome = self
            .insert_subject_in(&tx, subject, owner_user_id, &mut Budget::new(self))
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    async fn insert_subject_in(
        &self,
        db: &impl ConnectionTrait,
        subject: &EvalSubject,
        owner: &str,
        budget: &mut Budget<'_>,
    ) -> Result<SubjectInsertOutcome> {
        if !self.scope.is_legacy() {
            budget.charge(1, io::json_size(subject, io::MAX_BYTES)?)?;
        }
        validate_subject(subject)?;
        let content_digest = crate::storage::evaluation::subject_digest(subject)?;
        let subject_json = serde_json::to_string(subject)?;
        let key = self.key(KeyDomain::EvalSubject, &subject.eval_id)?;
        let metadata = self.scope.metadata(&self.provenance)?;
        let inserted = io::insert(
            db,
            subject_entity::Entity::insert(subject_entity::ActiveModel {
                installation_id: Set(metadata.installation),
                tenant_id: Set(metadata.tenant),
                creator_principal_id: Set(metadata.creator),
                eval_id: Set(key.clone()),
                owner_user_id: Set(owner.to_owned()),
                subject_id: Set(subject.subject_id.clone()),
                scope: Set(scope_name(subject.scope).into()),
                policy_digest: Set(subject.policy_digest.clone()),
                holdout: Set(subject.holdout),
                content_digest: Set(content_digest),
                subject_json: Set(subject_json),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
            })
            .into_query(),
            budget,
        )
        .await?;
        // Locked reads observe the committed winner even on MySQL. No uniqueness
        // conflict is acknowledged until its scoped content has been decoded.
        let row = io::one(
            db,
            self.subjects()
                .filter(subject_entity::Column::EvalId.eq(key))
                .lock_exclusive(),
            budget,
        )
        .await?
        .context("eval subject identity conflict")?;
        ensure!(
            row.owner_user_id == owner && self.decode_subject(&row)? == *subject,
            "eval subject immutable content conflict"
        );
        Ok(if inserted {
            SubjectInsertOutcome::Inserted
        } else {
            SubjectInsertOutcome::Duplicate
        })
    }

    pub async fn subject(&self, eval_id: &str) -> Result<Option<EvalSubject>> {
        self.read_subject(eval_id, None).await
    }

    pub async fn subject_for_owner(
        &self,
        eval_id: &str,
        owner_user_id: &str,
    ) -> Result<Option<EvalSubject>> {
        self.legacy_only()?;
        self.read_subject(eval_id, Some(owner_user_id)).await
    }

    async fn read_subject(
        &self,
        eval_id: &str,
        owner: Option<&str>,
    ) -> Result<Option<EvalSubject>> {
        let mut query = self
            .subjects()
            .filter(subject_entity::Column::EvalId.eq(self.key(KeyDomain::EvalSubject, eval_id)?));
        if let Some(owner) = owner {
            query = query.filter(subject_entity::Column::OwnerUserId.eq(owner));
        }
        let tx = self.transaction(false).await?;
        let value = io::one(&tx, query, &mut Budget::new(self))
            .await?
            .map(|row| {
                let subject = self.decode_subject(&row)?;
                ensure!(
                    subject.eval_id == eval_id,
                    "eval subject logical identity conflict"
                );
                Ok(subject)
            })
            .transpose()?;
        tx.commit().await?;
        Ok(value)
    }

    pub async fn list_subjects(&self) -> Result<Vec<EvalSubject>> {
        self.list_subjects_in_owner(None).await
    }

    pub async fn list_subjects_for_owner(&self, owner_user_id: &str) -> Result<Vec<EvalSubject>> {
        self.legacy_only()?;
        self.list_subjects_in_owner(Some(owner_user_id)).await
    }

    async fn list_subjects_in_owner(&self, owner: Option<&str>) -> Result<Vec<EvalSubject>> {
        let mut query = self.subjects().order_by_asc(subject_entity::Column::EvalId);
        if let Some(owner) = owner {
            query = query.filter(subject_entity::Column::OwnerUserId.eq(owner));
        }
        let tx = self.transaction(false).await?;
        let mut values = io::load(&tx, query, &mut Budget::new(self))
            .await?
            .iter()
            .map(|row| self.decode_subject(row))
            .collect::<Result<Vec<_>>>()?;
        if !self.scope.is_legacy() {
            values.sort_by(|a, b| a.eval_id.cmp(&b.eval_id));
        }
        tx.commit().await?;
        Ok(values)
    }

    pub async fn insert_result(&self, result: &EvaluationResult) -> Result<ResultInsertOutcome> {
        let owner = self.owner("local")?;
        let tx = self.transaction(true).await?;
        let outcome = self
            .insert_result_in(&tx, result, &owner, &mut Budget::new(self))
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    pub async fn insert_result_owned(
        &self,
        result: &EvaluationResult,
        owner_user_id: &str,
    ) -> Result<ResultInsertOutcome> {
        self.legacy_only()?;
        validate_owner(owner_user_id)?;
        let tx = self.transaction(true).await?;
        let outcome = self
            .insert_result_in(&tx, result, owner_user_id, &mut Budget::new(self))
            .await?;
        tx.commit().await?;
        Ok(outcome)
    }

    async fn insert_result_in(
        &self,
        db: &impl ConnectionTrait,
        result: &EvaluationResult,
        owner: &str,
        budget: &mut Budget<'_>,
    ) -> Result<ResultInsertOutcome> {
        if !self.scope.is_legacy() {
            io::json_size(result, io::MAX_BYTES)?;
        }
        validate_result(result)?;
        let eval_id = self.key(KeyDomain::EvalSubject, &result.eval_id)?;
        let subject = io::one(
            db,
            self.subjects()
                .filter(subject_entity::Column::EvalId.eq(&eval_id))
                .lock_exclusive(),
            budget,
        )
        .await?
        .context("unknown eval subject")?;
        ensure!(
            self.decode_subject(&subject)?.eval_id == result.eval_id
                && subject.owner_user_id == owner,
            "eval result parent identity mismatch"
        );
        if !self.scope.is_legacy() {
            budget.charge(1, io::json_size(result, io::MAX_BYTES)?)?;
        }
        let result_json = serde_json::to_string(result)?;
        let content_digest = crate::storage::evaluation::result_digest(result)?;
        let result_id = self.key(KeyDomain::EvalResult, &content_digest)?;
        let idempotency = self.key(KeyDomain::EvalIdempotency, &result.idempotency_key)?;
        let metadata = self.scope.metadata(&self.provenance)?;
        let inserted = io::insert(
            db,
            result_entity::Entity::insert(result_entity::ActiveModel {
                installation_id: Set(metadata.installation),
                tenant_id: Set(metadata.tenant),
                creator_principal_id: Set(metadata.creator),
                result_id: Set(result_id.clone()),
                owner_user_id: Set(owner.to_owned()),
                eval_id: Set(eval_id),
                idempotency_key: Set(idempotency.clone()),
                authority_id: Set(result.evaluator.authority_id.clone()),
                evaluator_kind: Set(format!("{:?}", result.evaluator.kind).to_ascii_lowercase()),
                content_digest: Set(content_digest.clone()),
                result_json: Set(result_json),
                created_at: Set(chrono::Utc::now().to_rfc3339()),
            })
            .into_query(),
            budget,
        )
        .await?;
        let row = io::one(
            db,
            self.results()
                .filter(result_entity::Column::OwnerUserId.eq(owner))
                .filter(result_entity::Column::IdempotencyKey.eq(idempotency))
                .lock_exclusive(),
            budget,
        )
        .await?
        .context("eval result identity conflict")?;
        let decoded = self.decode_result(&row)?;
        ensure!(
            row.result_id == result_id && decoded.result == *result,
            "eval result immutable content conflict"
        );
        Ok(if inserted {
            ResultInsertOutcome::Inserted {
                result_id: content_digest,
            }
        } else {
            ResultInsertOutcome::Duplicate {
                result_id: content_digest,
            }
        })
    }

    pub async fn result(&self, result_id: &str) -> Result<Option<StoredEvaluationResult>> {
        let tx = self.transaction(false).await?;
        let mut budget = Budget::new(self);
        let row = io::one(
            &tx,
            self.results().filter(
                result_entity::Column::ResultId.eq(self.key(KeyDomain::EvalResult, result_id)?),
            ),
            &mut budget,
        )
        .await?;
        let result = if let Some(row) = row {
            let value = self.checked_result(&tx, &row, &mut budget).await?;
            ensure!(
                value.result_id == result_id,
                "eval result logical identity conflict"
            );
            Some(value)
        } else {
            None
        };
        tx.commit().await?;
        Ok(result)
    }

    async fn checked_result(
        &self,
        db: &impl ConnectionTrait,
        row: &result_entity::Model,
        budget: &mut Budget<'_>,
    ) -> Result<StoredEvaluationResult> {
        let result = self.decode_result(row)?;
        let subject = io::one(
            db,
            self.subjects()
                .filter(subject_entity::Column::EvalId.eq(&row.eval_id)),
            budget,
        )
        .await?
        .context("stored eval result parent is unavailable")?;
        ensure!(
            self.decode_subject(&subject)?.eval_id == result.result.eval_id
                && subject.owner_user_id == row.owner_user_id,
            "stored eval result parent mismatch"
        );
        Ok(result)
    }

    pub async fn results_for_subject(&self, eval_id: &str) -> Result<Vec<StoredEvaluationResult>> {
        let tx = self.transaction(false).await?;
        let mut budget = Budget::new(self);
        let rows = io::load(
            &tx,
            self.results()
                .filter(
                    result_entity::Column::EvalId.eq(self.key(KeyDomain::EvalSubject, eval_id)?),
                )
                .order_by_asc(result_entity::Column::ResultId),
            &mut budget,
        )
        .await?;
        let mut values = Vec::with_capacity(rows.len());
        for row in &rows {
            let value = self.checked_result(&tx, row, &mut budget).await?;
            ensure!(
                value.result.eval_id == eval_id,
                "eval result logical parent conflict"
            );
            values.push(value);
        }
        if !self.scope.is_legacy() {
            values.sort_by(|a, b| a.result_id.cmp(&b.result_id));
        }
        tx.commit().await?;
        Ok(values)
    }

    pub async fn append_admission_event(
        &self,
        result_id: &str,
        status: AdmissionStatus,
        reason: &str,
        authority_id: &str,
    ) -> Result<AdmissionEvent> {
        let tx = self.transaction(true).await?;
        let event = self
            .append_admission_in(
                &tx,
                result_id,
                status,
                reason,
                authority_id,
                &mut Budget::new(self),
            )
            .await?;
        tx.commit().await?;
        Ok(event)
    }

    async fn append_admission_in(
        &self,
        db: &impl ConnectionTrait,
        result_id: &str,
        status: AdmissionStatus,
        reason: &str,
        authority_id: &str,
        budget: &mut Budget<'_>,
    ) -> Result<AdmissionEvent> {
        let key = self.key(KeyDomain::EvalResult, result_id)?;
        let row = io::one(
            db,
            self.results()
                .filter(result_entity::Column::ResultId.eq(&key))
                .lock_exclusive(),
            budget,
        )
        .await?
        .context("cannot admit unknown result")?;
        ensure!(
            self.checked_result(db, &row, budget).await?.result_id == result_id,
            "admission result identity conflict"
        );
        let metadata = self.scope.metadata(&self.provenance)?;
        budget.charge(
            1,
            reason
                .len()
                .checked_add(authority_id.len())
                .context("admission byte count overflow")?,
        )?;
        // Retrieve only the generated ID. ActiveModel::insert implicitly reloads
        // the whole row without scope on backends without INSERT RETURNING.
        // https://docs.rs/sea-orm/1.1.20/sea_orm/query/struct.Insert.html#method.exec
        budget.check()?;
        let inserted = admission_entity::Entity::insert(admission_entity::ActiveModel {
            installation_id: Set(metadata.installation),
            tenant_id: Set(metadata.tenant),
            creator_principal_id: Set(metadata.creator),
            sequence: Default::default(),
            result_id: Set(key),
            status: Set(admission_status_name(status).into()),
            reason: Set(reason.to_owned()),
            authority_id: Set(authority_id.to_owned()),
            created_at: Set(chrono::Utc::now().to_rfc3339()),
        })
        .exec(db)
        .await?;
        budget.check()?;
        let row = io::one(
            db,
            self.admissions()
                .filter(admission_entity::Column::Sequence.eq(inserted.last_insert_id))
                .lock_exclusive(),
            budget,
        )
        .await?
        .context("inserted admission is unavailable in this namespace")?;
        let event = self.decode_admission(&row, result_id)?;
        ensure!(
            event.status == status
                && event.reason == reason
                && event.authority_id == authority_id
                && row.creator_principal_id.as_deref()
                    == self.provenance.creator().map(|id| id.as_str()),
            "inserted admission content mismatch"
        );
        Ok(event)
    }

    pub async fn latest_admissions(&self) -> Result<BTreeMap<String, AdmissionEvent>> {
        self.read_latest(None).await
    }

    pub async fn latest_admissions_for_owner(
        &self,
        owner_user_id: &str,
    ) -> Result<BTreeMap<String, AdmissionEvent>> {
        self.legacy_only()?;
        self.read_latest(Some(owner_user_id)).await
    }

    async fn read_latest(&self, owner: Option<&str>) -> Result<BTreeMap<String, AdmissionEvent>> {
        let tx = self.transaction(false).await?;
        let values = self.latest_in(&tx, owner, &mut Budget::new(self)).await?;
        tx.commit().await?;
        Ok(values)
    }

    async fn latest_in(
        &self,
        db: &impl ConnectionTrait,
        owner: Option<&str>,
        budget: &mut Budget<'_>,
    ) -> Result<BTreeMap<String, AdmissionEvent>> {
        let mut query = self
            .admissions()
            .order_by_asc(admission_entity::Column::Sequence);
        if let Some(owner) = owner {
            let results = self
                .results()
                .select_only()
                .column(result_entity::Column::ResultId)
                .filter(result_entity::Column::OwnerUserId.eq(owner))
                .into_query();
            query = query.filter(
                sea_orm::sea_query::Expr::col(admission_entity::Column::ResultId)
                    .in_subquery(results),
            );
        }
        let mut latest = BTreeMap::new();
        for row in io::load(db, query, budget).await? {
            let result = io::one(
                db,
                self.results()
                    .filter(result_entity::Column::ResultId.eq(&row.result_id)),
                budget,
            )
            .await?
            .context("admission parent is unavailable")?;
            let result = self.checked_result(db, &result, budget).await?;
            let event = self.decode_admission(&row, &result.result_id)?;
            latest.insert(result.result_id, event);
        }
        Ok(latest)
    }
}

fn validate_owner(owner: &str) -> Result<()> {
    ensure!(
        !owner.trim().is_empty() && owner.len() <= 512,
        "eval owner user id must contain 1 to 512 characters"
    );
    Ok(())
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
        _ => anyhow::bail!("unknown stored admission status"),
    }
}

#[cfg(test)]
#[path = "store/scope_tests.rs"]
mod scope_tests;
#[cfg(test)]
#[path = "store/legacy_tests.rs"]
mod tests;
