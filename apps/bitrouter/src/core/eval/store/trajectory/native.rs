//! Read the exact operational record, including its original admission.
use super::*;
use crate::core::eval::compiler::EvalEvidenceRecord;

impl EvalStore {
    pub(crate) async fn native_publication_in(
        &self,
        tx: &DatabaseTransaction,
        eval_id: &str,
        digest: &str,
        shared: &OperationBudget<'_>,
    ) -> Result<EvalEvidenceRecord> {
        ensure!(
            !self.scope.is_legacy(),
            "native publication requires owned storage"
        );
        let mut budget = Budget::trajectory(self, shared);
        let subject = io::one(
            tx,
            self.subjects().filter(
                subject_entity::Column::EvalId.eq(self.key(KeyDomain::EvalSubject, eval_id)?),
            ),
            &mut budget,
        )
        .await?
        .context("native publication subject absent")?;
        let decoded_subject = self.decode_subject(&subject)?;
        let rows = io::load(
            tx,
            self.results()
                .filter(result_entity::Column::EvalId.eq(&subject.eval_id))
                .filter(result_entity::Column::ContentDigest.eq(digest)),
            &mut budget,
        )
        .await?;
        let [result] = rows.as_slice() else {
            anyhow::bail!("native publication result is not unique")
        };
        let decoded = self.decode_result(result)?;
        ensure!(
            decoded_subject.eval_id == eval_id
                && decoded.content_digest == digest
                && decoded.result.eval_id == eval_id
                && result.owner_user_id == subject.owner_user_id
                && result.creator_principal_id == subject.creator_principal_id,
            "native publication result parent changed"
        );
        crate::storage::evaluation::validate_result_for_subject(&decoded.result, &decoded_subject)?;
        crate::trajectory::evaluation::validate_builtin_trajectory_result(&decoded.result)?;
        let admissions = io::load(
            tx,
            self.admissions()
                .filter(admission_entity::Column::ResultId.eq(&result.result_id)),
            &mut budget,
        )
        .await?;
        let [admission] = admissions.as_slice() else {
            anyhow::bail!("native publication admission closure changed")
        };
        let admitted = self.decode_admission(admission, &decoded.result_id)?;
        ensure!(
            admission.creator_principal_id == subject.creator_principal_id
                && admitted.sequence > 0
                && admitted.status == AdmissionStatus::Admitted
                && admitted.reason == "admitted"
                && admitted.authority_id == decoded.result.evaluator.authority_id,
            "native publication is not the admitted operational result"
        );
        budget.check()?;
        Ok(EvalEvidenceRecord {
            result_id: decoded.result_id,
            content_digest: decoded.content_digest,
            subject: decoded_subject,
            result: decoded.result,
        })
    }
}
