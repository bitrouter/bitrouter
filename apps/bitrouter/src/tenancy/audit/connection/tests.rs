use super::*;
use crate::offline::{
    engine::Connections,
    large::{Options, Workspace},
};
use anyhow::{Context, Result, ensure};
use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};
use tokio_util::sync::CancellationToken;

mod directory;
mod operations;

#[tokio::test]
async fn typed_parameters_preserve_null_integer_and_text_values() -> Result<()> {
    let mut db = SqliteConnection::connect("sqlite::memory:").await?;
    let reader = ReadConnection::new(&mut db);
    let text = "a'\"?雪\0tail";
    let statement = Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT ? AS flag, ? AS small, ? AS large, ? AS unsigned, ? AS text, ? AS absent_text, ? AS absent_bool, ? AS absent_int, ? AS absent_unsigned, '?' AS literal",
        [
            true.into(),
            i32::MIN.into(),
            i64::MIN.into(),
            (i64::MAX as u64).into(),
            text.into(),
            Option::<String>::None.into(),
            Option::<bool>::None.into(),
            Option::<i64>::None.into(),
            Option::<u64>::None.into(),
        ],
    );
    let one = reader
        .query_one(statement.clone())
        .await?
        .context("missing row")?;
    let all = reader.query_all(statement).await?;
    ensure!(all.len() == 1);
    for row in std::iter::once(&one).chain(&all) {
        assert!(row.try_get::<bool>("", "flag")?);
        assert_eq!(row.try_get::<i32>("", "small")?, i32::MIN);
        assert_eq!(row.try_get::<i64>("", "large")?, i64::MIN);
        assert_eq!(row.try_get::<i64>("", "unsigned")?, i64::MAX);
        assert_eq!(row.try_get::<String>("", "text")?, text);
        assert_eq!(row.try_get::<Option<String>>("", "absent_text")?, None);
        for column in ["absent_bool", "absent_int", "absent_unsigned"] {
            assert_eq!(row.try_get::<Option<i64>>("", column)?, None);
        }
        assert_eq!(row.try_get::<String>("", "literal")?, "?");
    }
    let row = reader.query_one(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT length(CAST(? AS BLOB)) AS byte_len, CAST(? AS BLOB) AS data, ? AS empty, ? AS flag",
        [text.into(), text.into(), "".into(), false.into()],
    )).await?.context("missing byte witness")?;
    assert_eq!(
        usize::try_from(row.try_get::<i64>("", "byte_len")?)?,
        text.len()
    );
    assert_eq!(row.try_get::<Vec<u8>>("", "data")?, text.as_bytes());
    assert_eq!(row.try_get::<String>("", "empty")?, "");
    assert!(!row.try_get::<bool>("", "flag")?);
    drop(row);
    drop(reader);
    db.close().await?;
    Ok(())
}

#[test]
fn parameter_preflight_refuses_unsupported_overflow_and_working_capacity() {
    for value in [
        Value::Double(Some(1.0)),
        Value::Char(Some('x')),
        Value::Bytes(Some(Box::new(vec![1]))),
        Value::BigUnsigned(Some(u64::MAX)),
        Value::String(Some(Box::new(String::with_capacity(64 * 1024)))),
    ] {
        assert!(
            parameters(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT ?",
                [value],
            ))
            .is_err()
        );
    }
    // NULL parameters still allocate argument slots; payload bytes alone
    // cannot bound the conversion or the SQLx worker's owned arguments.
    assert!(
        parameters(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT ?",
            vec![Value::BigInt(None); 64 * 1024 / size_of::<Value>() + 1],
        ))
        .is_err()
    );
    assert!(
        parameters(Statement::from_string(
            DatabaseBackend::Sqlite,
            " ".repeat(64 * 1024 + 1),
        ))
        .is_err()
    );
    assert!(
        parameters(Statement::from_string(
            DatabaseBackend::Postgres,
            "SELECT 1"
        ))
        .is_err()
    );
}

#[tokio::test]
async fn read_errors_ack_clear_before_releasing_the_callers_allowance() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700))?;
    let workspace = Workspace::create(
        &Options {
            working_bytes: 16 * 1024 * 1024,
            scratch_directory: home.path().canonicalize()?,
            min_free_bytes: 1,
        },
        CancellationToken::new(),
    )?;
    let mut db = SqliteConnection::connect("sqlite::memory:").await?;
    let available = workspace.available();
    let caller_guard = workspace.reserve(available)?;
    let reader = ReadConnection::new(&mut db)
        .with_row_limit(1)
        .with_workspace(&workspace);
    assert!(
        reader
            .query_all(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT ? UNION ALL SELECT ?",
                [1_i32.into(), 2_i32.into()],
            ))
            .await
            .is_err()
    );
    assert!(
        reader
            .query_one(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT value FROM missing WHERE key=?",
                ["key".into()],
            ))
            .await
            .is_err()
    );
    let row = reader
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT ? AS value",
            ["still usable".into()],
        ))
        .await?
        .context("missing row after error")?;
    assert_eq!(row.try_get::<String>("", "value")?, "still usable");
    drop(row);
    drop(reader);
    assert_eq!(db.cached_statements_size(), 0);
    drop(caller_guard);
    assert_eq!(workspace.available(), available);
    db.close().await?;
    workspace.finish()?;
    Ok(())
}

#[tokio::test]
async fn abandoned_borrowed_query_quarantines_its_callers_working_guards() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700))?;
    let workspace = Workspace::create(
        &Options {
            working_bytes: 16 * 1024 * 1024,
            scratch_directory: home.path().canonicalize()?,
            min_free_bytes: 1,
        },
        CancellationToken::new(),
    )?;
    let path = home.path().join("borrowed.sqlite");
    std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(&path)?
        .sync_all()?;
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.open_budgeted(&path, true, &workspace).await?;
    let db = connections.copy.as_mut().context("copy missing")?;
    let entered = CancellationToken::new();
    let worker_entered = entered.clone();
    let (release, wait) = std::sync::mpsc::channel();
    let mut wait = Some(wait);
    db.lock_handle().await?.set_progress_handler(1, move || {
        if let Some(wait) = wait.take() {
            worker_entered.cancel();
            return wait.recv().is_ok();
        }
        true
    });
    let caller_guard = workspace.reserve(1024 * 1024)?;
    let reader = ReadConnection::new(db).with_workspace(&workspace);
    let mut query = Box::pin(reader.query_one(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "SELECT ?",
        ["owned argument".repeat(128).into()],
    )));
    ensure!(futures::poll!(&mut query).is_pending());
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.cancelled()).await?;
    drop(query);
    drop(caller_guard);
    ensure!(workspace.available() == 0 && workspace.reserve(1).is_err());
    drop(reader);
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("worker gate unavailable"))?;
    connections.close().await?;
    ensure!(workspace.available() == 0);
    workspace.finish()?;
    Ok(())
}
