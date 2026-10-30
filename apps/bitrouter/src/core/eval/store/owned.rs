//! Borrowed-transaction ledger operations. The tenancy actor owns all cleanup.

use std::sync::Arc;

use bitrouter_ai::eval::{AdmissionOutcome, EvaluatorKind, PutSubjectReport, StatusReport};
use sea_orm::DatabaseTransaction;

use super::*;
use crate::storage::evaluation::validate_result_for_subject;

pub(super) type Check<'a> = dyn Fn() -> std::result::Result<(), ErrorCode> + Sync + 'a;
type OwnedResult<T> = std::result::Result<T, OwnedError>;

struct Record {
    result: StoredEvaluationResult,
    subject: Arc<EvalSubject>,
    subject_digest: String,
    latest: AdmissionEvent,
}

impl EvalStore {
    fn owned_budget<'a>(&self, check: &'a Check<'a>) -> Result<Budget<'a>> {
        if self.scope.is_legacy() {
            return Err(OwnedError::Rejected(ErrorCode::Forbidden).into());
        }
        check().map_err(OwnedError::Rejected)?;
        Ok(Budget::owned(self, check))
    }

    pub(crate) async fn put_owned_in(
        &self,
        tx: &DatabaseTransaction,
        subject: &EvalSubject,
        check: &Check<'_>,
    ) -> OwnedResult<PutSubjectReport> {
        async {
            let mut budget = self.owned_budget(check)?;
            self.put_with_budget(tx, subject, &mut budget).await
        }
        .await
        .map_err(OwnedError::classify)
    }
    pub(super) async fn put_with_budget(
        &self,
        tx: &DatabaseTransaction,
        subject: &EvalSubject,
        budget: &mut Budget<'_>,
    ) -> Result<PutSubjectReport> {
        validate_subject(subject).map_err(|_| OwnedError::Rejected(ErrorCode::InvalidRequest))?;
        budget.charge(1, io::json_size(subject, io::MAX_BYTES)?)?;
        let row = io::one(
            tx,
            self.subjects().filter(
                subject_entity::Column::EvalId
                    .eq(self.key(KeyDomain::EvalSubject, &subject.eval_id)?),
            ),
            budget,
        )
        .await?;
        let outcome = if let Some(row) = row {
            if self.decode_subject(&row)? != *subject {
                return Err(OwnedError::Rejected(ErrorCode::InvalidRequest).into());
            }
            SubjectInsertOutcome::Duplicate
        } else {
            let report = PutSubjectReport {
                eval_id: subject.eval_id.clone(),
                outcome: SubjectInsertOutcome::Inserted,
            };
            budget.response(&report)?;
            let outcome = self
                .insert_subject_in(tx, subject, &self.owner("local")?, budget)
                .await?;
            ensure!(
                outcome == SubjectInsertOutcome::Inserted,
                "unexpected concurrent subject insertion"
            );
            return Ok(report);
        };
        let report = PutSubjectReport {
            eval_id: subject.eval_id.clone(),
            outcome,
        };
        budget.response(&report)?;
        Ok(report)
    }

    pub(crate) async fn submit_owned_in(
        &self,
        tx: &DatabaseTransaction,
        result: &EvaluationResult,
        check: &Check<'_>,
    ) -> OwnedResult<AdmissionOutcome> {
        async {
            let mut budget = self.owned_budget(check)?;
            if result.evaluator.kind != EvaluatorKind::Generic
                || self.provenance.creator().map(|id| id.as_str())
                    != Some(result.evaluator.authority_id.as_str())
                || !result.hard_violations.is_empty()
                // An operator may provision this literal principal name. Its
                // spelling must not mint the private built-in evaluator proof.
                || (result.evaluator.authority_id
                    == crate::trajectory::evaluation::TRAJECTORY_EVALUATOR_AUTHORITY_ID
                    && result.evaluator.evaluator_id
                        == crate::trajectory::evaluation::TRAJECTORY_EVALUATOR_ID)
            {
                return Err(OwnedError::Rejected(ErrorCode::Forbidden).into());
            }
            self.submit_with_budget(tx, result, &mut budget).await
        }
        .await
        .map_err(OwnedError::classify)
    }
    pub(super) async fn submit_with_budget(
        &self,
        tx: &DatabaseTransaction,
        result: &EvaluationResult,
        budget: &mut Budget<'_>,
    ) -> Result<AdmissionOutcome> {
        validate_result(result).map_err(|_| OwnedError::Rejected(ErrorCode::InvalidRequest))?;
        budget.charge(1, io::json_size(result, io::MAX_BYTES)?)?;
        let parent = io::one(
            tx,
            self.subjects().filter(
                subject_entity::Column::EvalId
                    .eq(self.key(KeyDomain::EvalSubject, &result.eval_id)?),
            ),
            budget,
        )
        .await?
        .ok_or(OwnedError::Rejected(ErrorCode::NotFound))?;
        let subject = self.decode_subject(&parent)?;
        validate_result_for_subject(result, &subject)
            .map_err(|_| OwnedError::Rejected(ErrorCode::InvalidRequest))?;
        let candidate = io::one(
            tx,
            self.results()
                .filter(result_entity::Column::OwnerUserId.eq(self.owner("local")?))
                .filter(
                    result_entity::Column::IdempotencyKey
                        .eq(self.key(KeyDomain::EvalIdempotency, &result.idempotency_key)?),
                ),
            budget,
        )
        .await?;
        if let Some(candidate) = candidate {
            let value = self.checked_result(tx, &candidate, budget).await?;
            if value.result != *result {
                return Err(OwnedError::Rejected(ErrorCode::InvalidRequest).into());
            }
            let latest = self.latest_owned_in(tx, &value.result_id, budget).await?;
            let response = AdmissionOutcome {
                result_id: value.result_id,
                status: latest.status,
                reason: latest.reason,
                duplicate: true,
            };
            budget.response(&response)?;
            return Ok(response);
        }
        let records = self
            .records_owned_in(
                tx,
                self.results()
                    .filter(result_entity::Column::EvalId.eq(&parent.eval_id)),
                budget,
            )
            .await?;
        let admitted = records
            .iter()
            .filter(|record| record.latest.status == AdmissionStatus::Admitted)
            .map(|record| &record.result)
            .collect::<Vec<_>>();
        let decision = crate::core::eval::admission::decide(result, subject.holdout, &admitted);
        let result_id = crate::storage::evaluation::result_digest(result)?;
        let response = AdmissionOutcome {
            result_id: result_id.clone(),
            status: decision.status,
            reason: decision.reason.into(),
            duplicate: false,
        };
        budget.response(&response)?;
        // Bound the complete append batch before any INSERT, including fixed
        // timestamp/metadata fields. Actual transfers are charged separately.
        budget.charge(decision.disputes.len() + 1, 0)?;
        for (id, reason) in decision
            .disputes
            .iter()
            .map(|(id, reason)| (id.as_str(), *reason))
            .chain(std::iter::once((result_id.as_str(), decision.reason)))
        {
            budget.charge(
                1,
                id.len() + reason.len() + result.evaluator.authority_id.len() + 512,
            )?;
        }
        budget.check()?;
        let inserted = self
            .insert_result_in(tx, result, &self.owner("local")?, budget)
            .await?;
        ensure!(
            matches!(inserted, ResultInsertOutcome::Inserted { .. }),
            "unexpected concurrent result insertion"
        );
        for (id, reason) in decision.disputes {
            self.append_admission_in(
                tx,
                &id,
                AdmissionStatus::Disputed,
                reason,
                &result.evaluator.authority_id,
                budget,
            )
            .await?;
        }
        self.append_admission_in(
            tx,
            &result_id,
            decision.status,
            decision.reason,
            &result.evaluator.authority_id,
            budget,
        )
        .await?;
        budget.check()?;
        Ok(response)
    }

    async fn latest_owned_in(
        &self,
        tx: &DatabaseTransaction,
        id: &str,
        budget: &mut Budget<'_>,
    ) -> Result<AdmissionEvent> {
        let row = io::one(
            tx,
            self.admissions()
                .filter(admission_entity::Column::ResultId.eq(self.key(KeyDomain::EvalResult, id)?))
                .order_by_desc(admission_entity::Column::Sequence),
            budget,
        )
        .await?
        .context("owned result has no terminal admission")?;
        self.decode_admission(&row, id)
    }

    async fn records_owned_in(
        &self,
        tx: &DatabaseTransaction,
        query: Select<result_entity::Entity>,
        budget: &mut Budget<'_>,
    ) -> Result<Vec<Record>> {
        let mut parents = BTreeMap::<String, (String, String, Arc<EvalSubject>)>::new();
        let mut records = Vec::new();
        for row in io::load(tx, query, budget).await? {
            budget.check()?;
            let result = self.decode_result(&row)?;
            if !parents.contains_key(&row.eval_id) {
                let parent = io::one(
                    tx,
                    self.subjects()
                        .filter(subject_entity::Column::EvalId.eq(&row.eval_id)),
                    budget,
                )
                .await?
                .context("owned result parent missing")?;
                let subject = self.decode_subject(&parent)?;
                parents.insert(
                    row.eval_id.clone(),
                    (
                        parent.owner_user_id,
                        parent.content_digest,
                        Arc::new(subject),
                    ),
                );
            }
            let (owner, digest, subject) = parents
                .get(&row.eval_id)
                .context("validated owned parent missing")?;
            ensure!(
                *owner == row.owner_user_id && subject.eval_id == result.result.eval_id,
                "owned result parent mismatch"
            );
            validate_result_for_subject(&result.result, subject)?;
            let latest = self.latest_owned_in(tx, &result.result_id, budget).await?;
            records.push(Record {
                result,
                subject: subject.clone(),
                subject_digest: digest.clone(),
                latest,
            });
        }
        records.sort_by(|a, b| a.result.result_id.cmp(&b.result.result_id));
        Ok(records)
    }

    pub(crate) async fn freeze_owned_in(
        &self,
        tx: &DatabaseTransaction,
        frozen_at: &str,
        check: &Check<'_>,
    ) -> OwnedResult<EvalSnapshot> {
        async {
            let mut budget = self.owned_budget(check)?;
            chrono::DateTime::parse_from_rfc3339(frozen_at)
                .map_err(|_| OwnedError::Rejected(ErrorCode::InvalidRequest))?;
            let records = self
                .records_owned_in(tx, self.results(), &mut budget)
                .await?;
            let entries = records
                .into_iter()
                .filter(|record| record.latest.status == AdmissionStatus::Admitted)
                .map(|record| EvalSnapshotEntry {
                    result_id: record.result.result_id,
                    content_digest: record.result.content_digest,
                    subject_content_digest: record.subject_digest,
                    eval_id: record.subject.eval_id.clone(),
                })
                .collect::<Vec<_>>();
            let snapshot = EvalSnapshot {
                evidence_root: self.snapshot_root(None, frozen_at, &entries)?,
                frozen_at: frozen_at.into(),
                entries,
            };
            self.verify_manifest(&snapshot, None)?;
            budget.response(&snapshot)?;
            // This exact manifest was built from validated closure in this tx.
            // It cannot be supplied by a caller or reused in another transaction.
            let prepared = self.prepare_manifest(&snapshot, &mut budget)?;
            self.persist_prepared_in(tx, prepared, None, &mut budget)
                .await
        }
        .await
        .map_err(OwnedError::classify)
    }

    pub(crate) async fn subjects_owned_in(
        &self,
        tx: &DatabaseTransaction,
        check: &Check<'_>,
    ) -> OwnedResult<Vec<EvalSubject>> {
        async {
            let mut budget = self.owned_budget(check)?;
            let mut subjects = io::load(tx, self.subjects(), &mut budget)
                .await?
                .iter()
                .map(|row| self.decode_subject(row))
                .collect::<Result<Vec<_>>>()?;
            subjects.sort_by(|a, b| a.eval_id.cmp(&b.eval_id));
            budget.response(&subjects)?;
            Ok(subjects)
        }
        .await
        .map_err(OwnedError::classify)
    }

    pub(crate) async fn subject_owned_in(
        &self,
        tx: &DatabaseTransaction,
        id: &str,
        check: &Check<'_>,
    ) -> OwnedResult<EvalSubject> {
        async {
            let mut budget = self.owned_budget(check)?;
            let row = io::one(
                tx,
                self.subjects().filter(
                    subject_entity::Column::EvalId.eq(self.key(KeyDomain::EvalSubject, id)?),
                ),
                &mut budget,
            )
            .await?
            .ok_or(OwnedError::Rejected(ErrorCode::NotFound))?;
            let subject = self.decode_subject(&row)?;
            ensure!(subject.eval_id == id, "owned subject identity mismatch");
            budget.response(&subject)?;
            Ok(subject)
        }
        .await
        .map_err(OwnedError::classify)
    }

    pub(crate) async fn snapshot_owned_in(
        &self,
        tx: &DatabaseTransaction,
        root: &str,
        check: &Check<'_>,
    ) -> OwnedResult<EvalSnapshot> {
        async {
            let mut budget = self.owned_budget(check)?;
            let (snapshot, owner) = self
                .read_snapshot_in(tx, root, None, &mut budget)
                .await?
                .ok_or(OwnedError::Rejected(ErrorCode::NotFound))?;
            self.manifest_records_in(tx, &snapshot, &mut budget, false, Some(&owner))
                .await?;
            budget.response(&snapshot)?;
            Ok(snapshot)
        }
        .await
        .map_err(OwnedError::classify)
    }

    pub(crate) async fn status_owned_in(
        &self,
        tx: &DatabaseTransaction,
        check: &Check<'_>,
    ) -> OwnedResult<StatusReport> {
        async {
            let mut budget = self.owned_budget(check)?;
            let subjects = io::load(tx, self.subjects(), &mut budget).await?;
            for row in &subjects {
                self.decode_subject(row)?;
            }
            let records = self
                .records_owned_in(tx, self.results(), &mut budget)
                .await?;
            let mut admission = BTreeMap::new();
            for record in &records {
                *admission
                    .entry(format!("{:?}", record.latest.status).to_ascii_lowercase())
                    .or_default() += 1;
            }
            let report = StatusReport {
                subjects: subjects.len(),
                results: records.len(),
                admission,
            };
            budget.response(&report)?;
            Ok(report)
        }
        .await
        .map_err(OwnedError::classify)
    }
}
