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
        for keys in page {
            let row = query::one_statement(db, point.statement(db, &keys)?, |row| {
                // The SQL guards already bound this row. Count its JSON nodes
                // before allocating canonical DTOs, using that same SQL row.
                if retained::memory::operation(row)? > body_bytes {
                    return Err(CapacityExceeded.into());
                }
                decode(row)
            })
            .await?
            .context("operation disappeared")?;
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
                        row.class == AuthorityClass::Tenant && row.tenant.as_ref() == Some(tenant),
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
}
