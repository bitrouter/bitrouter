//! Frozen request-subject storage for the current model-only shared gateway.
//! Not an authorization interface for external results.

use super::*;
impl SharedEvaluation {
    /// Persist a trusted frozen request subject in the caller's transaction.
    /// This neither creates a result/admission nor looks up current policy.
    /// The caller supplies the authenticated owner and holds gateway
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
    /// Replays use the same immutable subject verifier.
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
        let subject_digest = gateway_subject_digest(subject)?;
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
                 crate::storage::evaluation::identity_digest(&subject.identity)?.into(), subject.holdout.into(), subject_digest.into(), subject_json.into(), created_at.into()])?).await?;
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
