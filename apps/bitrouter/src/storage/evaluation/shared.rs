//! Namespace-scoped evaluation persistence for trusted request producers.
//!
//! Every operation borrows a Serializable transaction. The caller owns timeout,
//! rollback, whole-transaction retry and gateway locks. This module has
//! no pool, worker, transport or generic admission-authority API.

mod write;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::eval::{EvalSubject, EvalSubjectIdentity};
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseTransaction, QueryResult, Statement, Value,
};

use super::{subject_digest, validate_identifier};

const MAX_JSON_BYTES: usize = 1024 * 1024;

pub(crate) struct SharedEvaluation {
    namespace: String,
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
        row.map(|row| self.decode_subject(&row)).transpose()
    }

    /// Validate every standalone subject before a worker can acquire authority.
    /// Each keyset page transfers at most eight bounded JSON bodies. No row is
    /// selected by schema version, execution linkage, status or active revision.
    pub(crate) async fn validate_retained_subjects_in(
        &self,
        tx: &DatabaseTransaction,
    ) -> Result<()> {
        check_transaction(tx).await?;
        let mut after = String::new();
        loop {
            // C collation gives the same UTF-8 byte order as the cursor check.
            // Bound cells in SQL before the driver materializes untrusted data.
            let rows = tx.query_all(self.statement(tx,
                "SELECT namespace,
                    CASE WHEN octet_length(eval_id)<=512 THEN eval_id ELSE NULL END AS eval_id,
                    CASE WHEN octet_length(owner_user_id)<=512 THEN owner_user_id ELSE NULL END AS owner_user_id,
                    CASE WHEN octet_length(subject_id)<=512 THEN subject_id ELSE NULL END AS subject_id,
                    CASE WHEN octet_length(scope)<=32 THEN scope ELSE NULL END AS scope,
                    CASE WHEN octet_length(policy_digest)<=128 THEN policy_digest ELSE NULL END AS policy_digest,
                    holdout,
                    CASE WHEN octet_length(content_digest)<=128 THEN content_digest ELSE NULL END AS content_digest,
                    CASE WHEN octet_length(subject_json)<=$3 THEN subject_json ELSE NULL END AS subject_json,
                    CASE WHEN octet_length(created_at)<=64 THEN created_at ELSE NULL END AS created_at
                 FROM gateway_shared_eval_subjects
                 WHERE namespace=$1 AND ($2='' OR eval_id COLLATE \"C\">$2 COLLATE \"C\")
                 ORDER BY eval_id COLLATE \"C\" LIMIT 8",
                vec![after.as_str().into(), i64::try_from(MAX_JSON_BYTES)?.into()])?).await?;
            if rows.is_empty() {
                return Ok(());
            }
            for row in rows {
                let subject = self.decode_subject(&row)?;
                ensure!(subject.eval_id > after, "shared eval subject key order");
                after = subject.eval_id;
            }
        }
    }

    fn decode_subject(&self, row: &QueryResult) -> Result<EvalSubject> {
        let owner: String = row.try_get("", "owner_user_id")?;
        let id: String = row.try_get("", "eval_id")?;
        validate_scope(&owner, &id)?;
        self.check_row(row, &owner)?;
        let body: String = row.try_get("", "subject_json")?;
        ensure!(
            body.len() <= MAX_JSON_BYTES,
            "shared eval payload byte limit exceeded"
        );
        let subject: EvalSubject = serde_json::from_str(&body)?;
        let digest = gateway_subject_digest(&subject)?;
        ensure!(
            serde_json::to_string(&subject)? == body
                && subject.eval_id == id
                && subject.subject_id == row.try_get::<String>("", "subject_id")?
                && scope_name(subject.scope) == row.try_get::<String>("", "scope")?
                && crate::storage::evaluation::identity_digest(&subject.identity)?
                    == row.try_get::<String>("", "policy_digest")?
                && subject.holdout == row.try_get::<bool>("", "holdout")?
                && digest == row.try_get::<String>("", "content_digest")?,
            "shared eval subject differs from immutable evidence"
        );
        validate_timestamp(&row.try_get::<String>("", "created_at")?)?;
        Ok(subject)
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

fn gateway_subject_digest(subject: &EvalSubject) -> Result<String> {
    ensure!(
        matches!(subject.identity, EvalSubjectIdentity::GatewayGeneric {}),
        "shared eval requires gateway_generic identity"
    );
    subject_digest(subject)
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
