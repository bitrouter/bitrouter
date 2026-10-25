use super::*;

macro_rules! columns {
    ($model:ty, $($name:literal),+ $(,)?) => {
        impl io::Columns for $model {
            const NAMES: &'static [&'static str] = &["installation_id", "tenant_id", "creator_principal_id", $($name),+];
        }
    };
}
columns!(
    subject_entity::Model,
    "eval_id",
    "owner_user_id",
    "subject_id",
    "scope",
    "policy_digest",
    "holdout",
    "content_digest",
    "subject_json",
    "created_at"
);
columns!(
    result_entity::Model,
    "result_id",
    "owner_user_id",
    "eval_id",
    "idempotency_key",
    "authority_id",
    "evaluator_kind",
    "content_digest",
    "result_json",
    "created_at"
);
columns!(
    admission_entity::Model,
    "sequence",
    "result_id",
    "status",
    "reason",
    "authority_id",
    "created_at"
);
columns!(
    snapshot_entity::Model,
    "evidence_root",
    "owner_user_id",
    "manifest_json",
    "result_count",
    "frozen_at"
);

impl EvalStore {
    fn check_owner(&self, owner: &str) -> Result<()> {
        validate_owner(owner)?;
        if !self.scope.is_legacy() {
            ensure!(owner == self.owner("")?, "invalid eval compatibility owner");
        }
        Ok(())
    }

    pub(super) fn decode_subject(&self, row: &subject_entity::Model) -> Result<EvalSubject> {
        self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        self.check_owner(&row.owner_user_id)?;
        chrono::DateTime::parse_from_rfc3339(&row.created_at)?;
        let subject: EvalSubject =
            serde_json::from_str(&row.subject_json).context("parsing stored eval subject")?;
        ensure!(
            self.key(KeyDomain::EvalSubject, &subject.eval_id)? == row.eval_id
                && crate::storage::evaluation::subject_digest(&subject)? == row.content_digest
                && subject.subject_id == row.subject_id
                && scope_name(subject.scope) == row.scope
                && subject.policy_digest == row.policy_digest
                && subject.holdout == row.holdout,
            "stored eval subject indexed content mismatch"
        );
        Ok(subject)
    }

    pub(super) fn decode_result(
        &self,
        row: &result_entity::Model,
    ) -> Result<StoredEvaluationResult> {
        self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        self.check_owner(&row.owner_user_id)?;
        chrono::DateTime::parse_from_rfc3339(&row.created_at)?;
        let result: EvaluationResult =
            serde_json::from_str(&row.result_json).context("parsing stored evaluation result")?;
        let digest = crate::storage::evaluation::result_digest(&result)?;
        ensure!(
            self.key(KeyDomain::EvalSubject, &result.eval_id)? == row.eval_id
                && self.key(KeyDomain::EvalResult, &digest)? == row.result_id
                && digest == row.content_digest
                && self.key(KeyDomain::EvalIdempotency, &result.idempotency_key)?
                    == row.idempotency_key
                && result.evaluator.authority_id == row.authority_id
                && format!("{:?}", result.evaluator.kind).to_ascii_lowercase()
                    == row.evaluator_kind,
            "stored evaluation result indexed content mismatch"
        );
        Ok(StoredEvaluationResult {
            result_id: digest,
            content_digest: row.content_digest.clone(),
            result,
        })
    }

    pub(super) fn decode_admission(
        &self,
        row: &admission_entity::Model,
        logical_result: &str,
    ) -> Result<AdmissionEvent> {
        self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        ensure!(
            self.key(KeyDomain::EvalResult, logical_result)? == row.result_id,
            "admission result key mismatch"
        );
        chrono::DateTime::parse_from_rfc3339(&row.created_at)?;
        Ok(AdmissionEvent {
            sequence: row.sequence,
            result_id: logical_result.to_owned(),
            status: parse_admission_status(&row.status)?,
            reason: row.reason.clone(),
            authority_id: row.authority_id.clone(),
            created_at: row.created_at.clone(),
        })
    }

    pub(super) fn decode_snapshot(&self, row: &snapshot_entity::Model) -> Result<EvalSnapshot> {
        self.scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        self.check_owner(&row.owner_user_id)?;
        let snapshot: EvalSnapshot =
            serde_json::from_str(&row.manifest_json).context("parsing eval snapshot")?;
        ensure!(
            self.key(KeyDomain::EvalSnapshot, &snapshot.evidence_root)? == row.evidence_root
                && snapshot.frozen_at == row.frozen_at
                && i32::try_from(snapshot.entries.len()).ok() == Some(row.result_count),
            "stored eval snapshot indexed content mismatch"
        );
        self.verify_manifest(&snapshot, Some(&row.owner_user_id))?;
        Ok(snapshot)
    }
}
