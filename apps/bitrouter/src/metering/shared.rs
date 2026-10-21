//! Namespace-bound metering projections in the caller's authority transaction.
//! Local metering tables and their migration ledger are deliberately independent.

pub(crate) mod evidence;
pub(crate) mod record;
#[cfg(test)]
pub(crate) mod tests;

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseTransaction, QueryResult, Statement};
use sha2::{Digest, Sha256};

use record::{Accounting, Record, validate_id};

pub(crate) const MAX_ATTEMPTS: usize = 32;

/// Carries no pool or implicit transaction; projection/receipt/fence work commits
/// as one unit in the caller. The namespace comes from validated service authority.
pub(crate) struct SharedMetering {
    namespace: String,
}

/// Only already projected executions are included. A budget guard must also
/// establish that its authority scope has no missing or pending projections.
#[derive(Debug, PartialEq, Eq)]
#[cfg(test)]
pub(crate) struct ProjectedSpend {
    pub executions: u64,
    pub known_subtotal_micro_usd: u64,
    pub unknown_attempts: u64,
    /// A window total; an empty window has a derived total of zero. Individual
    /// no-dispatch executions retain absent monetary evidence in their records.
    pub complete_total_micro_usd: Option<u64>,
}

impl SharedMetering {
    pub fn new(namespace: &str) -> Result<Self> {
        ensure!(
            !namespace.is_empty()
                && namespace.len() <= 64
                && namespace
                    .bytes()
                    .all(|b| b.is_ascii_alphanumeric() || b"-_".contains(&b)),
            "invalid shared metering namespace"
        );
        Ok(Self {
            namespace: namespace.into(),
        })
    }

    /// Insert or verify exact replay. The immutable row is its own applied marker;
    /// it does not assert that trajectory/evaluation projection has completed.
    pub async fn apply_in(&self, tx: &DatabaseTransaction, record: &Record) -> Result<bool> {
        ensure_postgres(tx)?;
        let accounting = record.accounting()?;
        let body = serde_json::to_string(record)?;
        let execution = &record.event.execution;
        let digest = self.digest(record)?;
        // Conflict handling never overwrites an earlier projection, including
        // when a later caller offers a higher revision for the same terminal ID.
        // https://www.postgresql.org/docs/current/sql-insert.html#SQL-ON-CONFLICT
        let result = tx
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "INSERT INTO gateway_shared_metering_requests
             (namespace, execution_id, tenant_id, principal_id, request_id, event_id,
              source_revision, source_digest, accepted_at_ms, terminal_at_ms,
              known_subtotal_micro_usd, complete_total_micro_usd, known_attempts,
              unknown_attempts, not_dispatched_attempts, payload)
             VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11::text::numeric,$12::text::numeric,$13,$14,$15,$16)
             ON CONFLICT (namespace, execution_id) DO NOTHING",
                vec![
                    self.namespace.clone().into(),
                    execution.execution_id.clone().into(),
                    record.event.tenant_id.clone().into(),
                    record.principal_id.clone().into(),
                    record.request_id.clone().into(),
                    record.event.event_id.clone().into(),
                    i64::try_from(execution.revision)?.into(),
                    digest.into(),
                    execution.accepted_at_ms.into(),
                    execution.updated_at_ms.into(),
                    accounting.known_subtotal_micro_usd.to_string().into(),
                    accounting
                        .usage
                        .cost_micro_usd
                        .map(|value| value.to_string())
                        .into(),
                    i32::try_from(accounting.known_attempts)?.into(),
                    i32::try_from(accounting.unknown_attempts)?.into(),
                    i32::try_from(accounting.not_dispatched_attempts)?.into(),
                    body.clone().into(),
                ],
            ))
            .await?;
        let stored = self
            .get_in(
                tx,
                &record.event.tenant_id,
                &record.principal_id,
                &execution.execution_id,
            )
            .await?
            .context("conflicting metering projection owner")?;
        ensure!(
            stored == *record && serde_json::to_string(&stored)? == body,
            "conflicting immutable metering projection"
        );
        Ok(result.rows_affected() == 1)
    }

    pub async fn get_in(
        &self,
        tx: &DatabaseTransaction,
        tenant: &str,
        principal: &str,
        execution: &str,
    ) -> Result<Option<Record>> {
        ensure_postgres(tx)?;
        for id in [tenant, principal, execution] {
            validate_id(id)?;
        }
        let row = tx
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT namespace, execution_id, tenant_id, principal_id, request_id, event_id,
                    source_revision, source_digest, accepted_at_ms, terminal_at_ms,
                    known_subtotal_micro_usd::text AS known_subtotal_micro_usd,
                    complete_total_micro_usd::text AS complete_total_micro_usd,
                    known_attempts, unknown_attempts, not_dispatched_attempts, payload
             FROM gateway_shared_metering_requests
             WHERE namespace=$1 AND tenant_id=$2 AND principal_id=$3 AND execution_id=$4",
                [
                    self.namespace.clone().into(),
                    tenant.into(),
                    principal.into(),
                    execution.into(),
                ],
            ))
            .await?;
        row.map(|row| self.decode(row)).transpose()
    }

    /// Admission time owns the half-open window; projection/retry time cannot
    /// move spend between windows. Namespace and owner predicates precede SUM.
    #[cfg(test)]
    pub async fn spend_in(
        &self,
        tx: &DatabaseTransaction,
        tenant: &str,
        principal: Option<&str>,
        start_ms: i64,
        end_ms: i64,
    ) -> Result<ProjectedSpend> {
        ensure_postgres(tx)?;
        validate_id(tenant)?;
        if let Some(principal) = principal {
            validate_id(principal)?;
        }
        ensure!(
            start_ms >= 0 && end_ms > start_ms,
            "invalid metering window"
        );
        // PostgreSQL numeric aggregates avoid signed-i64 overflow; conversion
        // rejects values exceeding the public u64 accounting domain.
        // https://www.postgresql.org/docs/current/functions-aggregate.html
        let row = tx
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Postgres,
                "SELECT COUNT(*)::text AS executions,
                    COALESCE(SUM(known_subtotal_micro_usd),0)::text AS known,
                    COALESCE(SUM(unknown_attempts),0)::text AS unknown
             FROM gateway_shared_metering_requests
             WHERE namespace=$1 AND tenant_id=$2 AND ($3::text IS NULL OR principal_id=$3)
               AND accepted_at_ms >= $4 AND accepted_at_ms < $5",
                vec![
                    self.namespace.clone().into(),
                    tenant.into(),
                    principal.map(str::to_owned).into(),
                    start_ms.into(),
                    end_ms.into(),
                ],
            ))
            .await?
            .context("missing shared metering aggregate")?;
        let known = row.try_get::<String>("", "known")?.parse()?;
        let unknown = row.try_get::<String>("", "unknown")?.parse()?;
        Ok(ProjectedSpend {
            executions: row.try_get::<String>("", "executions")?.parse()?,
            known_subtotal_micro_usd: known,
            unknown_attempts: unknown,
            complete_total_micro_usd: (unknown == 0).then_some(known),
        })
    }

    fn digest(&self, record: &Record) -> Result<String> {
        // JSON tuple framing separates namespace, source format and evidence.
        Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
            "shared-metering-v1",
            &self.namespace,
            record,
        ))?)))
    }

    fn decode(&self, row: QueryResult) -> Result<Record> {
        let body: String = row.try_get("", "payload")?;
        ensure!(
            body.len() <= record::MAX_RECORD_BYTES,
            "oversized shared metering record"
        );
        let record: Record = serde_json::from_str(&body)?;
        let accounting = record.accounting()?;
        ensure!(
            serde_json::to_string(&record)? == body,
            "noncanonical metering record"
        );
        let execution = &record.event.execution;
        ensure!(
            row.try_get::<String>("", "namespace")? == self.namespace
                && row.try_get::<String>("", "execution_id")? == execution.execution_id
                && row.try_get::<String>("", "tenant_id")? == record.event.tenant_id
                && row.try_get::<String>("", "principal_id")? == record.principal_id
                && row.try_get::<Option<String>>("", "request_id")? == record.request_id
                && row.try_get::<String>("", "event_id")? == record.event.event_id
                && row.try_get::<i64>("", "source_revision")? == i64::try_from(execution.revision)?
                && row.try_get::<String>("", "source_digest")? == self.digest(&record)?
                && row.try_get::<i64>("", "accepted_at_ms")? == execution.accepted_at_ms
                && row.try_get::<i64>("", "terminal_at_ms")? == execution.updated_at_ms,
            "shared metering source columns differ from evidence"
        );
        validate_accounting_columns(&row, &accounting)?;
        Ok(record)
    }
}

fn validate_accounting_columns(row: &QueryResult, expected: &Accounting) -> Result<()> {
    let total = row
        .try_get::<Option<String>>("", "complete_total_micro_usd")?
        .map(|value| value.parse::<u64>())
        .transpose()?;
    ensure!(
        row.try_get::<String>("", "known_subtotal_micro_usd")?
            .parse::<u64>()?
            == expected.known_subtotal_micro_usd
            && total == expected.usage.cost_micro_usd
            && row.try_get::<i32>("", "known_attempts")? == i32::try_from(expected.known_attempts)?
            && row.try_get::<i32>("", "unknown_attempts")?
                == i32::try_from(expected.unknown_attempts)?
            && row.try_get::<i32>("", "not_dispatched_attempts")?
                == i32::try_from(expected.not_dispatched_attempts)?,
        "shared metering accounting columns differ from evidence"
    );
    Ok(())
}

fn ensure_postgres(tx: &DatabaseTransaction) -> Result<()> {
    ensure!(
        tx.get_database_backend() == DatabaseBackend::Postgres,
        "shared metering requires PostgreSQL"
    );
    Ok(())
}
