//! The built-in authority is obtainable only from an acknowledged trajectory start.
use super::*;
use crate::{
    storage::operation_budget::OperationBudget, tenancy::trajectory::StartProof,
    trajectory::types::Settlement,
};
use sea_orm::DatabaseTransaction;

impl EvalStore {
    /// A terminal replay may only observe its original publication. In
    /// particular, a missing row must not enter the normal insertion path,
    /// even when the caller would subsequently roll that transaction back.
    pub(crate) async fn check_trajectory_in(
        &self,
        tx: &DatabaseTransaction,
        proof: &StartProof,
        settlement: &Settlement,
        shared: &OperationBudget<'_>,
    ) -> std::result::Result<(), OwnedError> {
        async {
            let mut budget = Budget::trajectory(self, shared);
            budget.check()?;
            let envelope = proof.publication(&self.scope, &self.provenance, settlement)?;
            let creator = self.provenance.creator().map(|id| id.as_str());
            let subject = io::one(
                tx,
                self.subjects().filter(
                    subject_entity::Column::EvalId
                        .eq(self.key(KeyDomain::EvalSubject, &envelope.subject.eval_id)?),
                ),
                &mut budget,
            )
            .await?
            .context("trajectory replay subject missing")?;
            ensure!(
                subject.creator_principal_id.as_deref() == creator
                    && self.decode_subject(&subject)? == envelope.subject,
                "trajectory replay subject mismatch"
            );
            let result = io::one(
                tx,
                self.results()
                    .filter(result_entity::Column::OwnerUserId.eq(self.owner("local")?))
                    .filter(result_entity::Column::IdempotencyKey.eq(
                        self.key(KeyDomain::EvalIdempotency, &envelope.result.idempotency_key)?,
                    )),
                &mut budget,
            )
            .await?
            .context("trajectory replay result missing")?;
            let decoded = self.decode_result(&result)?;
            ensure!(
                result.creator_principal_id.as_deref() == creator
                    && result.eval_id == subject.eval_id
                    && result.owner_user_id == subject.owner_user_id
                    && decoded.result == envelope.result,
                "trajectory replay result mismatch"
            );
            let admissions = io::load(
                tx,
                self.admissions()
                    .filter(admission_entity::Column::ResultId.eq(&result.result_id)),
                &mut budget,
            )
            .await?;
            // Built-in operational results are Inconclusive and cannot acquire
            // dispute history through manual Eval. Their closure is one admission.
            let [admission] = admissions.as_slice() else {
                anyhow::bail!("trajectory replay admission closure mismatch");
            };
            let decoded = self.decode_admission(admission, &decoded.result_id)?;
            ensure!(
                admission.creator_principal_id.as_deref() == creator
                    && decoded.sequence > 0
                    && decoded.status == AdmissionStatus::Admitted
                    && decoded.reason == "admitted"
                    && decoded.authority_id == envelope.result.evaluator.authority_id,
                "trajectory replay admission mismatch"
            );
            budget.check()
        }
        .await
        .map_err(OwnedError::classify)
    }

    pub(crate) async fn publish_trajectory_in(
        &self,
        tx: &DatabaseTransaction,
        proof: &StartProof,
        settlement: &Settlement,
        shared: &OperationBudget<'_>,
    ) -> std::result::Result<bool, OwnedError> {
        async {
            let envelope = proof.publication(&self.scope, &self.provenance, settlement)?;
            let mut budget = Budget::trajectory(self, shared);
            let subject = self
                .put_with_budget(tx, &envelope.subject, &mut budget)
                .await?;
            let result = self
                .submit_with_budget(tx, &envelope.result, &mut budget)
                .await?;
            ensure!(
                result.status == AdmissionStatus::Admitted && result.reason == "admitted",
                "built-in trajectory result lost fixed admission"
            );
            budget.check()?;
            Ok(subject.outcome == SubjectInsertOutcome::Inserted || !result.duplicate)
        }
        .await
        .map_err(OwnedError::classify)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn manual_principal_spelling_cannot_mint_builtin_authority() -> Result<()> {
        use crate::trajectory::evaluation::{
            TRAJECTORY_EVALUATOR_AUTHORITY_ID, TRAJECTORY_EVALUATOR_ID,
        };
        use bitrouter_ai::eval::{EvalVerdict, EvaluatorKind};
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let (scope, provenance) = Scope::for_test(
            "installation".try_into()?,
            "tenant".try_into()?,
            TRAJECTORY_EVALUATOR_AUTHORITY_ID.try_into()?,
        );
        let store = EvalStore {
            db: db.clone(),
            scope,
            provenance,
        };
        store
            .insert_subject(&super::super::tests::subject()?)
            .await?;
        let mut result = super::super::tests::result(EvalVerdict::Inconclusive);
        result.evaluator.kind = EvaluatorKind::Generic;
        result.evaluator.authority_id = TRAJECTORY_EVALUATOR_AUTHORITY_ID.into();
        result.evaluator.evaluator_id = TRAJECTORY_EVALUATOR_ID.into();
        let tx = store.transaction(false).await?;
        let check = || Ok(());
        assert!(matches!(
            store.submit_owned_in(&tx, &result, &check).await,
            Err(OwnedError::Rejected(ErrorCode::Forbidden))
        ));
        // Same transaction and principal remain usable for ordinary manual Eval.
        result.evaluator.evaluator_id = "manual-observer".into();
        assert_eq!(
            store.submit_owned_in(&tx, &result, &check).await?.status,
            AdmissionStatus::Admitted
        );
        tx.rollback().await?;
        assert!(store.results_for_subject("eval-1").await?.is_empty());
        db.close().await?;
        Ok(())
    }
    #[tokio::test]
    async fn trajectory_and_eval_use_one_aggregate_budget() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let (scope, provenance) = Scope::for_test(
            "installation".try_into()?,
            "tenant".try_into()?,
            "creator".try_into()?,
        );
        let store = EvalStore {
            db: db.clone(),
            scope,
            provenance,
        };
        let check = || Ok(());
        let shared = OperationBudget::new(&check);
        let mut trajectory = crate::trajectory::store::owned::io::Budget::owned(&shared);
        let mut eval = Budget::trajectory(&store, &shared);
        trajectory.charge(3000, 31 * 1024 * 1024)?;
        eval.charge(1096, 1024 * 1024)?;
        ensure!(matches!(
            OwnedError::classify(
                eval.charge(0, 1)
                    .err()
                    .context("separate Eval byte budget escaped aggregate")?
            ),
            OwnedError::Rejected(ErrorCode::CapacityExceeded)
        ));
        ensure!(trajectory.charge(1, 0).is_err());
        db.close().await?;
        Ok(())
    }
}
