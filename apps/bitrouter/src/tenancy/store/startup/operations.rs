use super::*;
use crate::tenancy::{
    model::{AuthorityClass, ReadContext},
    store::{Operation, StoredResponse},
};
use bitrouter_ai::tenancy::{
    identity::{Digest, InstallationId, OperationId, PrincipalId, TenantId},
    operation::OperationStatus,
    validation::Validate,
};
use sea_orm::{DatabaseBackend, Statement};

const KEYS: [&str; 3] = ["installation_id", "principal_id", "operation_id"];

enum Point {
    Sqlite { sql: String, bytes: i64 },
    Other(Box<SelectStatement>),
}

impl Point {
    fn new(db: &impl ConnectionTrait, bytes: usize) -> Result<Self> {
        if bytes == 0 || bytes > isize::MAX as usize {
            return Err(CapacityExceeded.into());
        }
        if db.get_database_backend() == DatabaseBackend::Sqlite {
            Ok(Self::Sqlite {
                sql: query::sqlite_operation(),
                bytes: i64::try_from(bytes).map_err(|_| CapacityExceeded)?,
            })
        } else {
            Ok(Self::Other(Box::new(query::select(
                db,
                Table::Operation,
                bytes,
            )?)))
        }
    }

    fn statement(&self, db: &impl ConnectionTrait, keys: &[String]) -> Result<Statement> {
        let [installation, principal, operation] = keys else {
            anyhow::bail!("invalid operation point key");
        };
        match self {
            Self::Sqlite { sql, bytes } => Ok(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                sql.clone(),
                [
                    installation.as_str().into(),
                    principal.as_str().into(),
                    operation.as_str().into(),
                    (*bytes).into(),
                ],
            )),
            Self::Other(projection) => {
                let mut selected = projection.as_ref().clone();
                for (key, value) in KEYS.iter().zip(keys) {
                    selected.and_where(Expr::col(Alias::new(*key)).eq(value.as_str()));
                }
                selected.limit(2);
                Ok(db.get_database_backend().build(&selected))
            }
        }
    }
}

struct Row {
    installation: InstallationId,
    tenant: Option<TenantId>,
    class: AuthorityClass,
    operation: Operation,
}

fn decode(row: &QueryResult) -> Result<Row> {
    let principal: PrincipalId = super::super::string(row, "principal_id")?.try_into()?;
    let id: OperationId = super::super::string(row, "operation_id")?.try_into()?;
    Ok(Row {
        installation: super::super::string(row, "installation_id")?.try_into()?,
        tenant: row
            .try_get::<Option<String>>("", "tenant_id")?
            .map(TryInto::try_into)
            .transpose()?,
        class: super::super::enum_from(row, "authority_class")?,
        operation: super::super::operation_from(row, &principal, &id)?,
    })
}

fn result(operation: &Operation, header: &AuthorityHeader) -> Result<()> {
    let result = &operation.result;
    ensure!(
        result.operation_id == operation.id
            && result
                .authority_revision
                .is_none_or(|revision| revision <= header.revision),
        "operation result identity or authority mismatch"
    );
    let _: Digest = operation.digest.clone().try_into()?;
    match &operation.response {
        StoredResponse::Operation(returned) => ensure!(
            serde_json::to_value(returned)? == serde_json::to_value(result)?,
            "operation response differs from result"
        ),
        StoredResponse::Issued(issued) => {
            issued.validate()?;
            ensure!(
                issued.secret.is_none()
                    && issued.operation_id == operation.id
                    && result.status == OperationStatus::Applied
                    && Some(issued.authority_revision) == result.authority_revision
                    && Some(&issued.credential_revision) == result.resource_revision.as_ref()
                    && result.resource_id.as_ref().map(|id| id.as_str())
                        == Some(issued.credential_id.as_str()),
                "issuance response differs from result"
            );
        }
        StoredResponse::Configuration(configuration) => {
            configuration.validate()?;
            ensure!(
                result.status == OperationStatus::Applied
                    && result.resource_revision.as_ref() == Some(&configuration.revision_id),
                "configuration response differs from result"
            );
        }
        StoredResponse::Failure(failure) => {
            failure.validate()?;
            ensure!(
                result.status == OperationStatus::Rejected
                    && failure.operation_id.as_ref() == Some(&operation.id)
                    && serde_json::to_value(
                        result.error.as_ref().context("missing rejected result")?
                    )? == serde_json::to_value(failure)?,
                "failure response differs from result"
            );
        }
    }
    Ok(())
}

fn increment(count: &mut u64) -> Result<()> {
    *count = count
        .checked_add(1)
        .filter(|value| *value <= i64::MAX as u64)
        .context("operation counter exceeds SQL representation")?;
    Ok(())
}

fn batch_allowance(body_bytes: usize, rows: usize) -> Option<(usize, usize)> {
    if rows == 0 || rows > query::OPERATION_BATCH_ROWS {
        return None;
    }
    // Additional row/vector and query scratch, never borrowed from the retained
    // key page. The extra slots include the n+1 duplicate witness.
    let fixed = rows
        .checked_add(4)?
        .checked_mul(4096)?
        .checked_add(4 * 64 * 1024)?;
    let usable = body_bytes.checked_sub(fixed)?;
    let encoded = usable.checked_div(rows.checked_mul(12)?)?;
    (encoded > 0).then_some((encoded, usable / 2))
}

async fn read_batch(
    db: &impl ConnectionTrait,
    keys: &[Vec<String>],
    body_bytes: usize,
) -> Result<Option<Vec<QueryResult>>> {
    if db.get_database_backend() != DatabaseBackend::Sqlite {
        return Ok(None);
    }
    let Some((encoded, decoded)) = batch_allowance(body_bytes, keys.len()) else {
        return Ok(None);
    };
    let Some(statement) = query::sqlite_operation_batch(keys, encoded)? else {
        return Ok(None);
    };
    let rows = db.query_all(statement).await?;
    // The n+1 query-only witness fits usable, but may exceed its encoded half.
    // Reject it before any JSON preflight or canonical allocation.
    ensure!(
        rows.len() == keys.len(),
        "operation batch identity count mismatch"
    );
    let mut fits = true;
    for row in &rows {
        ensure!(
            row.try_get::<i32>("", "directory_valid")? == 1,
            "invalid directory row storage types"
        );
        let bytes = usize::try_from(row.try_get::<i64>("", "directory_bytes")?)?;
        match row.try_get::<i32>("", "directory_fits")? {
            0 => fits = false,
            1 => ensure!(bytes <= encoded, "operation batch byte guard mismatch"),
            _ => anyhow::bail!("invalid operation batch size marker"),
        }
    }
    if !fits {
        return Ok(None);
    }
    for (row, keys) in rows.iter().zip(keys) {
        for (column, expected) in KEYS.iter().zip(keys) {
            ensure!(
                row.try_get::<String>("", column)? == *expected,
                "operation batch key mismatch"
            );
        }
    }
    for row in &rows {
        if retained::memory::operation(row)? > decoded {
            return Ok(None);
        }
    }
    // Every row has passed preflight before counters/cursor advance. On any
    // None above, rows and ephemeral SQL allocations drop inside this helper,
    // before the caller starts its unchanged full-body point fallback.
    Ok(Some(rows))
}

pub(super) async fn validate_in(
    db: &impl ConnectionTrait,
    header: &AuthorityHeader,
    budget: Budget,
) -> Result<()> {
    let page_budget = budget.page()?;
    let key_bytes = budget
        .rows
        .checked_add(4)
        .and_then(|rows| rows.checked_mul(4096))
        .ok_or(CapacityExceeded)?;
    let body_bytes = budget
        .bytes
        .checked_sub(key_bytes)
        .ok_or(CapacityExceeded)?;
    let mut after: Option<Vec<String>> = None;
    let mut ordinary = 0_u64;
    let mut safety = 0_u64;
    let bytes = page_budget.bytes / page_budget.rows;
    // SQLite retains only a fixed text template, never a prepared worker/cache
    // entry. Other backends keep their existing guarded projection semantics.
    let point = Point::new(db, body_bytes / 6)?;
    loop {
        let page = query::read(
            db,
            retained::key_page(db, Table::Operation, &KEYS, after.as_deref(), page_budget)?,
            |row| {
                KEYS.iter()
                    .map(|key| super::super::string(row, key))
                    .collect::<Result<Vec<_>>>()
            },
        )
        .await?;
        if page.is_empty() {
            break;
        }
        for keys in page.chunks(query::OPERATION_BATCH_ROWS) {
            let batch = read_batch(db, keys, body_bytes).await?;
            for (index, keys) in keys.iter().enumerate() {
                let row = if let Some(rows) = &batch {
                    decode(rows.get(index).context("operation batch row missing")?)?
                } else {
                    query::one_statement(db, point.statement(db, keys)?, |row| {
                        // The SQL guards already bound this row. Count its JSON nodes
                        // before allocating canonical DTOs, using that same SQL row.
                        if retained::memory::operation(row)? > body_bytes {
                            return Err(CapacityExceeded.into());
                        }
                        decode(row)
                    })
                    .await?
                    .context("operation disappeared")?
                };
                ensure!(
                    row.installation == header.installation,
                    "operation history installation mismatch"
                );
                let operation = &row.operation;
                result(operation, header)?;
                match &operation.context {
                    ReadContext::Installation => ensure!(
                        row.class == AuthorityClass::Installation && row.tenant.is_none(),
                        "operation read context differs from indexed owner"
                    ),
                    ReadContext::Tenant { tenant, scopes, .. } => {
                        ensure!(
                            row.class == AuthorityClass::Tenant
                                && row.tenant.as_ref() == Some(tenant),
                            "operation read context differs from indexed owner"
                        );
                        for scope in scopes {
                            scope.validate()?;
                        }
                        ensure!(
                            directory::tenant_in(db, tenant, bytes).await?.is_some()
                                && directory::principal_in(db, &operation.principal, bytes)
                                    .await?
                                    .is_some(),
                            "orphan operation read context"
                        );
                    }
                }
                increment(if operation.safety {
                    &mut safety
                } else {
                    &mut ordinary
                })?;
                after = Some(vec![
                    row.installation.to_string(),
                    operation.principal.to_string(),
                    operation.id.to_string(),
                ]);
            }
        }
    }
    ensure!(
        ordinary == header.operation_ids && safety == header.safety_operation_ids,
        "operation history disagrees with retained counters"
    );
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn operation_point_rejects_invalid_limits_and_preserves_four_typed_parameters()
    -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        for bytes in [
            0,
            (isize::MAX as usize)
                .checked_add(1)
                .context("test limit overflow")?,
        ] {
            let error = Point::new(&db, bytes)
                .err()
                .context("invalid limit accepted")?;
            assert!(error.is::<CapacityExceeded>());
        }
        let point = Point::new(&db, 4096)?;
        assert!(point.statement(&db, &[]).is_err());
        let statement = point.statement(
            &db,
            &[
                "installation".into(),
                "principal".into(),
                "operation".into(),
            ],
        )?;
        assert_eq!(statement.db_backend, DatabaseBackend::Sqlite);
        assert_eq!(
            statement.values.context("parameters missing")?.0,
            vec![
                "installation".into(),
                "principal".into(),
                "operation".into(),
                4096_i64.into(),
            ]
        );
        assert!(statement.sql.ends_with("LIMIT 2"));
        db.close().await?;
        Ok(())
    }

    #[test]
    fn counters_cross_u32_and_refuse_sql_overflow() -> Result<()> {
        let mut count = u64::from(u32::MAX);
        increment(&mut count)?;
        assert_eq!(count, u64::from(u32::MAX) + 1);
        count = i64::MAX as u64;
        assert!(increment(&mut count).is_err());
        assert_eq!(count, i64::MAX as u64);
        Ok(())
    }

    #[tokio::test]
    async fn operation_batch_rejects_extra_rows_before_storage_and_json_preflight() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        db.execute_unprepared("CREATE TABLE tenancy_operations(installation_id,principal_id,operation_id,tenant_id,authority_class,read_context_json,command_digest,result_json,response_json,created_at,hash_version,safety)").await?;
        let keys = vec![vec![
            "installation".into(),
            "principal".into(),
            "operation".into(),
        ]];
        let insert = "INSERT INTO tenancy_operations VALUES('installation','principal','operation',NULL,'installation','{}','digest','{}','not-json','created',1,0)";
        db.execute_unprepared(insert).await?;
        db.execute_unprepared(insert).await?;
        for mixed_storage in [false, true] {
            if mixed_storage {
                db.execute_unprepared("UPDATE tenancy_operations SET response_json=NULL WHERE rowid=(SELECT max(rowid) FROM tenancy_operations)").await?;
            }
            let error = read_batch(&db, &keys, 1024 * 1024)
                .await
                .err()
                .context("duplicate accepted")?;
            assert!(
                error
                    .to_string()
                    .contains("operation batch identity count mismatch"),
                "{error:#}"
            );
        }
        db.execute_unprepared("DELETE FROM tenancy_operations WHERE rowid=(SELECT min(rowid) FROM tenancy_operations)").await?;
        let error = read_batch(&db, &keys, 1024 * 1024)
            .await
            .err()
            .context("invalid storage accepted")?;
        assert!(
            error
                .to_string()
                .contains("invalid directory row storage types"),
            "{error:#}"
        );
        db.execute_unprepared("DELETE FROM tenancy_operations")
            .await?;
        let error = read_batch(&db, &keys, 1024 * 1024)
            .await
            .err()
            .context("missing operation accepted")?;
        assert!(
            error
                .to_string()
                .contains("operation batch identity count mismatch"),
            "{error:#}"
        );
        // The extra witness can occupy more than the encoded half, but remains
        // within usable because the count guard runs before canonical preflight.
        let (encoded, decoded) = batch_allowance(1024 * 1024, 1).context("batch allowance")?;
        assert!(encoded * 6 * 2 > decoded && encoded * 6 * 2 <= decoded * 2);
        db.close().await?;
        Ok(())
    }

    #[tokio::test]
    async fn operation_batch_budget_miss_does_not_query_or_mask_real_read_errors() -> Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        let keys = vec![vec![
            "installation".into(),
            "principal".into(),
            "operation".into(),
        ]];
        // There is deliberately no table. Optional scratch pressure must select
        // fallback before SQL; with enough room the actual SELECT error is kept.
        assert!(read_batch(&db, &keys, 128 * 1024).await?.is_none());
        let error = read_batch(&db, &keys, 1024 * 1024)
            .await
            .err()
            .context("SQL failure converted to a fast-path miss")?;
        assert!(!error.is::<CapacityExceeded>());
        db.close().await?;
        Ok(())
    }
}
