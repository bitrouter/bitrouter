//! Namespace-scoped evaluation persistence for trusted trajectory producers.
//!
//! Every operation borrows a Serializable transaction. The caller owns timeout,
//! rollback, whole-transaction retry and gateway/episode locks. This module has
//! no pool, worker, transport or generic admission-authority API.

mod write;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::eval::{AdmissionStatus, EvalSubject, EvaluationResult};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, QueryResult, Statement, Value,
};

use super::{result_digest, subject_digest, validate_identifier, validate_result_for_subject};

const MAX_JSON_BYTES: usize = 1024 * 1024;

pub(crate) struct SharedEvaluation {
    namespace: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SharedAdmission {
    pub sequence: i64,
    pub result_id: String,
    pub status: AdmissionStatus,
    pub reason: String,
    pub authority_id: String,
    pub created_at: String,
}

impl SharedEvaluation {
    pub fn new(namespace: &str) -> Result<Self> {
        ensure!(
            !namespace.is_empty()
                && namespace.len() <= 64
                && namespace
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "invalid shared evaluation namespace"
        );
        Ok(Self {
            namespace: namespace.into(),
        })
    }

    pub async fn subject_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
    ) -> Result<Option<EvalSubject>> {
        check_transaction(tx).await?;
        let row = self.bounded_row(tx, owner, id,
            "SELECT namespace,owner_user_id,octet_length(subject_json) AS bytes FROM gateway_shared_eval_subjects
             WHERE namespace=$1 AND owner_user_id=$2 AND eval_id=$3",
            "SELECT * FROM gateway_shared_eval_subjects WHERE namespace=$1 AND owner_user_id=$2 AND eval_id=$3").await?;
        row.map(|row| {
            let body: String = row.try_get("", "subject_json")?;
            let subject: EvalSubject = serde_json::from_str(&body)?;
            let digest = subject_digest(&subject)?;
            ensure!(
                serde_json::to_string(&subject)? == body
                    && subject.eval_id == row.try_get::<String>("", "eval_id")?
                    && subject.subject_id == row.try_get::<String>("", "subject_id")?
                    && scope_name(subject.scope) == row.try_get::<String>("", "scope")?
                    && subject.policy_digest == row.try_get::<String>("", "policy_digest")?
                    && subject.holdout == row.try_get::<bool>("", "holdout")?
                    && digest == row.try_get::<String>("", "content_digest")?,
                "shared eval subject differs from immutable evidence"
            );
            validate_timestamp(&row.try_get::<String>("", "created_at")?)?;
            Ok(subject)
        })
        .transpose()
    }

    pub async fn result_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
    ) -> Result<Option<EvaluationResult>> {
        check_transaction(tx).await?;
        let row = self.bounded_row(tx, owner, id,
            "SELECT namespace,owner_user_id,octet_length(result_json) AS bytes FROM gateway_shared_eval_results
             WHERE namespace=$1 AND owner_user_id=$2 AND result_id=$3",
            "SELECT * FROM gateway_shared_eval_results WHERE namespace=$1 AND owner_user_id=$2 AND result_id=$3").await?;
        let Some(row) = row else {
            return Ok(None);
        };
        let body: String = row.try_get("", "result_json")?;
        let result: EvaluationResult = serde_json::from_str(&body)?;
        let digest = result_digest(&result)?;
        ensure!(
            serde_json::to_string(&result)? == body
                && digest == row.try_get::<String>("", "result_id")?
                && digest == row.try_get::<String>("", "content_digest")?
                && result.eval_id == row.try_get::<String>("", "eval_id")?
                && result.idempotency_key == row.try_get::<String>("", "idempotency_key")?
                && result.evaluator.authority_id == row.try_get::<String>("", "authority_id")?
                && format!("{:?}", result.evaluator.kind).to_ascii_lowercase()
                    == row.try_get::<String>("", "evaluator_kind")?,
            "shared eval result differs from immutable evidence"
        );
        validate_timestamp(&row.try_get::<String>("", "created_at")?)?;
        let subject = self
            .subject_in(tx, owner, &result.eval_id)
            .await?
            .context("shared eval result lost its owned subject")?;
        validate_result_for_subject(&result, &subject)?;
        Ok(Some(result))
    }

    pub async fn latest_admission_in(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        result_id: &str,
    ) -> Result<Option<SharedAdmission>> {
        check_transaction(tx).await?;
        validate_scope(owner, result_id)?;
        // Filter namespace/owner before ordering. Select the latest state before
        // examining status; an old Admitted row must not hide a later dispute.
        // https://www.postgresql.org/docs/current/queries-order.html
        let row = tx.query_one(self.statement(tx,
            "SELECT a.*,r.owner_user_id FROM gateway_shared_eval_results r
             JOIN gateway_shared_eval_admission_events a ON a.namespace=r.namespace AND a.result_id=r.result_id
             WHERE r.namespace=$1 AND r.owner_user_id=$2 AND r.result_id=$3 ORDER BY a.sequence DESC LIMIT 1",
            vec![owner.into(), result_id.into()])?).await?;
        row.map(|row| {
            self.check_row(&row, owner)?;
            let admission = SharedAdmission {
                sequence: row.try_get("", "sequence")?,
                result_id: row.try_get("", "result_id")?,
                status: match row.try_get::<String>("", "status")?.as_str() {
                    "admitted" => AdmissionStatus::Admitted,
                    "rejected" => AdmissionStatus::Rejected,
                    "held_out" => AdmissionStatus::HeldOut,
                    "disputed" => AdmissionStatus::Disputed,
                    _ => anyhow::bail!("unknown shared eval admission status"),
                },
                reason: row.try_get("", "reason")?,
                authority_id: row.try_get("", "authority_id")?,
                created_at: row.try_get("", "created_at")?,
            };
            ensure!(
                admission.sequence > 0 && admission.result_id == result_id,
                "invalid shared eval admission identity"
            );
            validate_identifier(&admission.reason, "admission reason")?;
            validate_identifier(&admission.authority_id, "admission authority")?;
            validate_timestamp(&admission.created_at)?;
            Ok(admission)
        })
        .transpose()
    }

    async fn bounded_row(
        &self,
        tx: &DatabaseTransaction,
        owner: &str,
        id: &str,
        metadata_sql: &'static str,
        payload_sql: &'static str,
    ) -> Result<Option<QueryResult>> {
        validate_scope(owner, id)?;
        let values = vec![owner.into(), id.into()];
        let metadata = tx
            .query_one(self.statement(tx, metadata_sql, values.clone())?)
            .await?;
        let Some(metadata) = metadata else {
            return Ok(None);
        };
        self.check_row(&metadata, owner)?;
        ensure!(
            usize::try_from(metadata.try_get::<i32>("", "bytes")?)? <= MAX_JSON_BYTES,
            "shared eval payload byte limit exceeded"
        );
        let row = tx
            .query_one(self.statement(tx, payload_sql, values)?)
            .await?
            .context("shared eval snapshot changed")?;
        self.check_row(&row, owner)?;
        Ok(Some(row))
    }

    fn statement(
        &self,
        tx: &DatabaseTransaction,
        sql: &'static str,
        values: Vec<Value>,
    ) -> Result<Statement> {
        ensure!(
            tx.get_database_backend() == DatabaseBackend::Postgres,
            "shared evaluation requires PostgreSQL"
        );
        let mut scoped = Vec::with_capacity(values.len() + 1);
        scoped.push(self.namespace.clone().into());
        scoped.extend(values);
        Ok(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            sql,
            scoped,
        ))
    }

    fn check_row(&self, row: &QueryResult, owner: &str) -> Result<()> {
        ensure!(
            row.try_get::<String>("", "namespace")? == self.namespace
                && row.try_get::<String>("", "owner_user_id")? == owner,
            "shared evaluation namespace or owner mismatch"
        );
        Ok(())
    }
}

fn validate_scope(owner: &str, id: &str) -> Result<()> {
    validate_identifier(owner, "evaluation owner")?;
    validate_identifier(id, "evaluation identity")
}

fn validate_timestamp(value: &str) -> Result<()> {
    ensure!(
        value.len() <= 64,
        "shared evaluation timestamp is oversized"
    );
    chrono::DateTime::parse_from_rfc3339(value).context("invalid shared evaluation timestamp")?;
    Ok(())
}

fn scope_name(scope: bitrouter_ai::eval::EvalScope) -> &'static str {
    match scope {
        bitrouter_ai::eval::EvalScope::Request => "request",
        bitrouter_ai::eval::EvalScope::Episode => "episode",
        bitrouter_ai::eval::EvalScope::Task => "task",
    }
}

async fn check_transaction(tx: &DatabaseTransaction) -> Result<()> {
    ensure!(
        tx.get_database_backend() == DatabaseBackend::Postgres,
        "shared evaluation requires PostgreSQL"
    );
    let row = tx
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SHOW transaction_isolation".to_owned(),
        ))
        .await?
        .context("missing transaction isolation")?;
    ensure!(
        row.try_get::<String>("", "transaction_isolation")? == "serializable",
        "shared evaluation requires Serializable isolation"
    );
    Ok(())
}

async fn admission_time(tx: &DatabaseTransaction) -> Result<String> {
    // Actual successful application time, not envelope creation or retry time.
    // https://www.postgresql.org/docs/current/functions-datetime.html#FUNCTIONS-DATETIME-CURRENT
    let row = tx
        .query_one(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT FLOOR(EXTRACT(EPOCH FROM clock_timestamp()) * 1000)::bigint AS now_ms"
                .to_owned(),
        ))
        .await?
        .context("missing database clock")?;
    Ok(
        chrono::DateTime::<chrono::Utc>::from_timestamp_millis(row.try_get("", "now_ms")?)
            .context("database clock outside timestamp range")?
            .to_rfc3339(),
    )
}
