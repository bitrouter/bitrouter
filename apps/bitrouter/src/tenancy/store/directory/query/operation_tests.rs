use super::*;
use anyhow::Context;
use sea_orm::{DatabaseConnection, Value};
use std::cell::Cell;

const KEYS: [&str; 3] = ["installation_id", "principal_id", "operation_id"];
const IDENTITY: [&str; 3] = ["installation", "principal", "operation"];

async fn database() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    // Deliberately no column affinity/NOT NULL: exercise corrupt storage types
    // which the point projection must reject before any canonical allocation.
    let columns = Table::Operation
        .columns()
        .iter()
        .chain(Table::Operation.integers())
        .map(|(name, _)| format!("\"{name}\""))
        .collect::<Vec<_>>()
        .join(",");
    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        format!("CREATE TABLE tenancy_operations ({columns})"),
    ))
    .await?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "CREATE INDEX operation_point ON tenancy_operations(installation_id,principal_id,operation_id)"
    )).await?;
    insert(&db).await?;
    Ok(db)
}

async fn insert(db: &DatabaseConnection) -> Result<()> {
    insert_key(db, IDENTITY).await
}

async fn insert_key(db: &DatabaseConnection, identity: [&str; 3]) -> Result<()> {
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_operations VALUES(?,?,?,?,?,?,?,?,?,?,?,?)",
        [
            identity[0].into(),
            identity[1].into(),
            identity[2].into(),
            Option::<String>::None.into(),
            "installation".into(),
            "{}".into(),
            "digest".into(),
            "{}".into(),
            "雪\0tail".into(),
            "created".into(),
            1_i64.into(),
            0_i64.into(),
        ],
    ))
    .await?;
    Ok(())
}

fn compact(sql: &str, limit: usize, identity: [&str; 3]) -> Result<Statement> {
    Ok(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        sql,
        [
            identity[0].into(),
            identity[1].into(),
            identity[2].into(),
            i64::try_from(limit)?.into(),
        ],
    ))
}

fn original(db: &DatabaseConnection, limit: usize) -> Result<Statement> {
    let mut selected = select(db, Table::Operation, limit)?;
    for (key, value) in KEYS.iter().zip(IDENTITY) {
        selected.and_where(Expr::col(Alias::new(*key)).eq(value));
    }
    selected.limit(2);
    Ok(db.get_database_backend().build(&selected))
}

fn payload(row: &QueryResult) -> Result<String> {
    Ok(row.try_get("", "response_json")?)
}

fn capacity(result: Result<Option<String>>) -> Result<()> {
    let error = result.err().context("oversized row accepted")?;
    ensure!(error.is::<super::super::CapacityExceeded>(), "{error:#}");
    Ok(())
}

#[tokio::test]
async fn compact_operation_preserves_rows_aggregate_bytes_and_missing_identity() -> Result<()> {
    let db = database().await?;
    let sql = sqlite_operation();
    let wide = compact(&sql, 4096, IDENTITY)?;
    let row = db
        .query_one(wide.clone())
        .await?
        .context("fixture missing")?;
    let bytes = usize::try_from(row.try_get::<i64>("", "directory_bytes")?)?;
    let expected = [
        "installation",
        "principal",
        "operation",
        "installation",
        "{}",
        "digest",
        "{}",
        "雪\0tail",
        "created",
    ]
    .iter()
    .map(|value| value.len())
    .sum::<usize>()
        + 16;
    assert_eq!(bytes, expected);
    assert_eq!(row.try_get::<Option<String>>("", "tenant_id")?, None);
    assert_eq!(payload(&row)?.as_bytes(), "雪\0tail".as_bytes());
    for statement in [original(&db, bytes)?, compact(&sql, bytes, IDENTITY)?] {
        assert_eq!(
            one_statement(&db, statement, payload).await?.as_deref(),
            Some("雪\0tail")
        );
    }
    // Each cell fits independently at N-1; the aggregate must still refuse.
    for statement in [
        original(&db, bytes - 1)?,
        compact(&sql, bytes - 1, IDENTITY)?,
    ] {
        capacity(one_statement(&db, statement, payload).await)?;
    }
    let rejected = db
        .query_one(compact(&sql, bytes - 1, IDENTITY)?)
        .await?
        .context("sentinel missing")?;
    for (column, _) in Table::Operation.columns() {
        assert_eq!(rejected.try_get::<Option<String>>("", column)?, None);
    }
    for (column, _) in Table::Operation.integers() {
        assert_eq!(rejected.try_get::<Option<i64>>("", column)?, None);
    }
    assert!(
        one_statement(
            &db,
            compact(&sql, bytes, [IDENTITY[0], IDENTITY[1], "missing"])?,
            payload
        )
        .await?
        .is_none()
    );
    // Four bound scalars plus the smaller SQL stay in the existing input
    // envelope. This is a fixed template, never retained per operation key.
    let values = wide.values.as_ref().context("arguments missing")?;
    assert_eq!(values.0.len(), 4);
    let parameter_bytes = values.0.capacity() * size_of::<Value>()
        + values
            .0
            .iter()
            .map(|value| match value {
                Value::String(Some(text)) => text.capacity() + size_of::<String>(),
                _ => 0,
            })
            .sum::<usize>();
    assert!(sql.capacity() + wide.sql.capacity() + parameter_bytes < 64 * 1024);
    assert!(wide.sql.len() < original(&db, 4096)?.sql.len());
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn compact_operation_preserves_storage_corruption_and_null_sentinels() -> Result<()> {
    let db = database().await?;
    let sql = sqlite_operation();
    for assignment in [
        "response_json=NULL",
        "response_json=x'00'",
        "tenant_id=x'00'",
        "hash_version=NULL",
        "hash_version='1'",
        "safety=NULL",
        "safety=2",
    ] {
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            "DELETE FROM tenancy_operations",
        ))
        .await?;
        insert(&db).await?;
        db.execute(Statement::from_string(
            DatabaseBackend::Sqlite,
            format!("UPDATE tenancy_operations SET {assignment}"),
        ))
        .await?;
        for statement in [original(&db, 4096)?, compact(&sql, 4096, IDENTITY)?] {
            let row = db
                .query_one(statement.clone())
                .await?
                .context("corrupt row was hidden")?;
            assert_eq!(row.try_get::<i32>("", "directory_valid")?, 0);
            assert_eq!(row.try_get::<Option<String>>("", "response_json")?, None);
            let decoded = Cell::new(false);
            let error = one_statement(&db, statement, |_| {
                decoded.set(true);
                Ok(())
            })
            .await
            .err()
            .context("corrupt storage accepted")?;
            assert!(!decoded.get());
            assert!(!error.is::<super::super::CapacityExceeded>());
            assert!(
                error
                    .to_string()
                    .contains("invalid directory row storage types")
            );
        }
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn compact_operation_probes_both_indexed_branches_and_rejects_mixed_duplicates() -> Result<()>
{
    let db = database().await?;
    let sql = sqlite_operation();
    let statement = compact(&sql, 4096, IDENTITY)?;
    let plan = db
        .query_all(Statement {
            sql: format!("EXPLAIN QUERY PLAN {}", statement.sql),
            ..statement.clone()
        })
        .await?;
    let details = plan
        .iter()
        .map(|row| row.try_get::<String>("", "detail"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let searches = details
        .iter()
        .filter(|detail| {
            detail.contains("SEARCH tenancy_operations")
                && detail.contains("operation_point")
                && detail.contains("installation_id=? AND principal_id=? AND operation_id=?")
        })
        .count();
    assert_eq!(searches, 2, "{details:?}");
    assert!(
        details
            .iter()
            .all(|detail| !detail.contains("SCAN tenancy_operations")
                && !detail.contains("TEMP B-TREE")),
        "{details:?}"
    );
    insert(&db).await?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "UPDATE tenancy_operations SET response_json=NULL WHERE rowid=(SELECT max(rowid) FROM tenancy_operations)"
    )).await?;
    let rows = db.query_all(statement.clone()).await?;
    assert_eq!(rows.len(), 2);
    let mut validity = rows
        .iter()
        .map(|row| row.try_get::<i32>("", "directory_valid"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    validity.sort_unstable();
    assert_eq!(validity, [0, 1]);
    for statement in [original(&db, 4096)?, statement] {
        let decoded = Cell::new(false);
        let error = one_statement(&db, statement, |_| {
            decoded.set(true);
            Ok(())
        })
        .await
        .err()
        .context("duplicate accepted")?;
        assert!(!decoded.get());
        assert!(
            error
                .to_string()
                .contains("duplicate indexed authority identity")
        );
    }
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn operation_batch_guards_every_key_and_keeps_the_extra_duplicate_marker() -> Result<()> {
    let db = database().await?;
    let second = [IDENTITY[0], IDENTITY[1], "z-operation"];
    insert_key(&db, second).await?;
    let keys = [IDENTITY, second].map(|key| key.map(str::to_owned).to_vec());
    let statement = sqlite_operation_batch(&keys, 4096)?.context("batch declined")?;
    let rows = db.query_all(statement.clone()).await?;
    assert_eq!(rows.len(), 2);
    for (row, key) in rows.iter().zip(&keys) {
        assert_eq!(row.try_get::<String>("", "operation_id")?, key[2]);
        assert_eq!(payload(row)?.as_bytes(), "雪\0tail".as_bytes());
    }
    // First OR member must still pass the aggregate/type predicates. The
    // malformed branch transfers scalar markers, never the BLOB or large text.
    for (body, valid) in [
        (Value::Bytes(Some(Box::new(vec![0; 8192]))), 0),
        ("x".repeat(8192).into(), 1),
    ] {
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "UPDATE tenancy_operations SET response_json=? WHERE operation_id=?",
            [body, IDENTITY[2].into()],
        ))
        .await?;
        let rows = db.query_all(statement.clone()).await?;
        assert_eq!(rows.len(), 2);
        let rejected = rows.first().context("missing marker")?;
        assert_eq!(rejected.try_get::<i32>("", "directory_valid")?, valid);
        assert_eq!(rejected.try_get::<i32>("", "directory_fits")?, 0);
        assert_eq!(
            rejected.try_get::<Option<String>>("", "response_json")?,
            None
        );
        assert_eq!(
            payload(rows.get(1).context("valid peer missing")?)?.as_bytes(),
            "雪\0tail".as_bytes()
        );
    }
    insert(&db).await?;
    let rows = db.query_all(statement).await?;
    assert_eq!(rows.len(), 3, "compound LIMIT must retain the n+1 witness");
    let mut fits = rows
        .iter()
        .map(|row| row.try_get::<i32>("", "directory_fits"))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    fits.sort_unstable();
    assert_eq!(fits, [0, 1, 1]);
    db.close().await?;
    Ok(())
}

#[test]
fn operation_batch_input_checks_owned_capacity_and_strict_key_order() -> Result<()> {
    let key = IDENTITY.map(str::to_owned).to_vec();
    assert!(sqlite_operation_batch(&[key.clone(), key.clone()], 4096).is_err());
    let mut reversed = vec![
        key.clone(),
        vec!["a".into(), "principal".into(), "operation".into()],
    ];
    assert!(sqlite_operation_batch(&reversed, 4096).is_err());
    reversed.reverse();
    let mut statement = sqlite_operation_batch(&reversed, 4096)?.context("valid keys declined")?;
    assert!(operation_batch_input_fits(&statement));
    // Length alone is insufficient: retaining a large SQL allocation must
    // decline the optional path before the actual adapter is called.
    statement.sql.reserve(64 * 1024);
    assert!(!operation_batch_input_fits(&statement));
    Ok(())
}
