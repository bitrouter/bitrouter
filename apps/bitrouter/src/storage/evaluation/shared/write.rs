//! Frozen request-subject storage and narrow built-in trajectory admission.
//! Not an authorization interface for external results.

use super::*;
use crate::trajectory::evaluation::{
    TrajectoryEvaluationEnvelope, validate_builtin_trajectory_result, validate_evaluation_envelope,
};
use bitrouter_ai::eval::AdmissionOutcome;

impl SharedEvaluation {
    /// The caller supplies a trusted durable trajectory envelope and holds any
    /// gateway/episode locks first. Subject serialization follows those locks.
    pub async fn apply_trajectory_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        envelope: &TrajectoryEvaluationEnvelope,
    ) -> Result<AdmissionOutcome> {
        validate_identifier(owner, "evaluation owner")?;
        ensure!(
            serde_json::to_vec(envelope)?.len() <= MAX_JSON_BYTES,
            "shared evaluation envelope byte limit exceeded"
        );
        validate_evaluation_envelope(envelope)?;
        validate_builtin_trajectory_result(&envelope.result)?;
        ensure!(
            !envelope.subject.holdout,
            "shared trajectory publication cannot admit a holdout subject"
        );
        let result = &envelope.result;
        let result_id = result_digest(result)?;
        self.insert_subject_in(tx, owner, &envelope.subject).await?;

        let existing = tx
            .query_one(self.statement(
                tx,
                "SELECT namespace,owner_user_id,result_id FROM gateway_shared_eval_results
             WHERE namespace=$1 AND owner_user_id=$2 AND idempotency_key=$3",
                vec![owner.into(), result.idempotency_key.clone().into()],
            )?)
            .await?;
        if let Some(row) = existing {
            self.check_row(&row, owner)?;
            ensure!(
                row.try_get::<String>("", "result_id")? == result_id,
                "conflicting shared evaluation idempotency key"
            );
            ensure!(
                self.result_in(tx, owner, &result_id).await?.as_ref() == Some(result),
                "conflicting shared evaluation result"
            );
            let admission = self
                .latest_admission_in(tx, owner, &result_id)
                .await?
                .context("existing shared evaluation result has no admission")?;
            return Ok(AdmissionOutcome {
                result_id,
                status: admission.status,
                reason: admission.reason,
                duplicate: true,
            });
        }
        // A different binding of the content identity is a conflict, not a new
        // owner's result or a license to reconstruct missing indexed fields.
        let collision = tx.query_one(self.statement(tx,
            "SELECT namespace FROM gateway_shared_eval_results WHERE namespace=$1 AND result_id=$2",
            vec![result_id.clone().into()])?).await?;
        ensure!(
            collision.is_none(),
            "shared evaluation result identity is already bound"
        );
        let admitted_at = admission_time(tx).await?;
        tx.execute(self.statement(tx,
            "INSERT INTO gateway_shared_eval_results
             (namespace,result_id,owner_user_id,eval_id,idempotency_key,authority_id,evaluator_kind,content_digest,result_json,created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$2,$8,$9)",
            vec![result_id.clone().into(), owner.into(), result.eval_id.clone().into(), result.idempotency_key.clone().into(),
                 result.evaluator.authority_id.clone().into(), format!("{:?}", result.evaluator.kind).to_ascii_lowercase().into(),
                 serde_json::to_string(result)?.into(), admitted_at.clone().into()])?).await?;
        tx.execute(self.statement(tx,
            "INSERT INTO gateway_shared_eval_admission_events (namespace,result_id,status,reason,authority_id,created_at)
             VALUES ($1,$2,'admitted','admitted',$3,$4)",
            vec![result_id.clone().into(), result.evaluator.authority_id.clone().into(), admitted_at.into()])?).await?;
        Ok(AdmissionOutcome {
            result_id,
            status: AdmissionStatus::Admitted,
            reason: "admitted".into(),
            duplicate: false,
        })
    }

    /// Persist a trusted frozen request subject in the caller's transaction.
    /// This neither creates a result/admission nor looks up current policy.
    /// The caller supplies the authenticated owner and holds gateway/episode
    /// locks first; rollback and whole-transaction retry remain caller-owned.
    /// Holdout is retained as evidence, not interpreted as admission here.
    pub async fn insert_request_subject_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        subject: &bitrouter_ai::eval::EvalSubject,
    ) -> Result<()> {
        ensure!(
            subject.scope == bitrouter_ai::eval::EvalScope::Request,
            "shared request subject requires Request scope"
        );
        self.insert_subject_in(tx, owner, subject).await
    }

    /// Serialize creators/replays on the namespace-qualified subject identity.
    /// Both request storage and trajectory admission use this exact verifier.
    async fn insert_subject_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        subject: &bitrouter_ai::eval::EvalSubject,
    ) -> Result<()> {
        validate_identifier(owner, "evaluation owner")?;
        let subject_json = serde_json::to_string(subject)?;
        ensure!(
            subject_json.len() <= MAX_JSON_BYTES,
            "shared evaluation subject byte limit exceeded"
        );
        // Validates schema, identities, observed_at, redaction and evidence
        // digests before any statement. Stored-row timestamps are verified by
        // subject_in below, including on an otherwise identical replay.
        let subject_digest = subject_digest(subject)?;
        check_transaction(tx).await?;
        let created_at = admission_time(tx).await?;
        // Upsert does not overwrite evidence. Concurrent creation aborts/retries
        // the entire outer Serializable transaction when required.
        // https://www.postgresql.org/docs/current/sql-insert.html#SQL-ON-CONFLICT
        tx.execute(self.statement(tx,
            "INSERT INTO gateway_shared_eval_subjects
             (namespace,eval_id,owner_user_id,subject_id,scope,policy_digest,holdout,content_digest,subject_json,created_at)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10) ON CONFLICT (namespace,eval_id) DO NOTHING",
            vec![subject.eval_id.clone().into(), owner.into(), subject.subject_id.clone().into(), scope_name(subject.scope).into(),
                 subject.policy_digest.clone().into(), subject.holdout.into(), subject_digest.into(), subject_json.into(), created_at.into()])?).await?;
        let locked = tx.query_one(self.statement(tx,
            "SELECT namespace,owner_user_id FROM gateway_shared_eval_subjects WHERE namespace=$1 AND eval_id=$2 FOR UPDATE",
            vec![subject.eval_id.clone().into()])?).await?.context("shared evaluation subject disappeared")?;
        self.check_row(&locked, owner)?;
        ensure!(
            self.subject_in(tx, owner, &subject.eval_id).await?.as_ref() == Some(subject),
            "conflicting shared evaluation subject"
        );
        Ok(())
    }
}
