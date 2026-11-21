use super::*;
use crate::offline::inventory;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};

#[tokio::test]
async fn schema_expression_is_refused_before_integrity_evaluates_it() -> Result<()> {
    let mut db = SqliteConnectOptions::new()
        .in_memory(true)
        .connect()
        .await?;
    let calls = Arc::new(AtomicUsize::new(0));
    let observed = calls.clone();
    db.lock_handle()
        .await?
        .create_collation("inspection_probe", move |a, b| {
            observed.fetch_add(1, Ordering::SeqCst);
            a.cmp(b)
        })?;
    sea_orm::sqlx::query(
        "CREATE TABLE probe(value TEXT CHECK(value COLLATE inspection_probe != 'forbidden'))",
    )
    .execute(&mut db)
    .await?;
    sea_orm::sqlx::query("INSERT INTO probe VALUES('small')")
        .execute(&mut db)
        .await?;
    calls.store(0, Ordering::SeqCst);
    policy(
        &mut db,
        CancellationToken::new(),
        Some(Instant::now() + Duration::from_secs(10)),
    )
    .await?;
    ensure!(inventory::inspect(&mut db, &[9; 32]).await.is_err());
    ensure!(
        calls.load(Ordering::SeqCst) == 0,
        "integrity evaluated unqualified CHECK"
    );
    // Positive control: the actual same CHECK is evaluated by SQLite integrity.
    let integrity: String = sea_orm::sqlx::query_scalar("PRAGMA integrity_check(1)")
        .fetch_one(&mut db)
        .await?;
    ensure!(integrity == "ok" && calls.load(Ordering::SeqCst) > 0);
    db.close().await?;
    Ok(())
}

#[test]
fn tiny_expanding_checks_and_defaults_are_not_an_executable_profile() -> Result<()> {
    for sql in [
        "CREATE TABLE x(a CHECK(length(randomblob(1))>0))",
        "CREATE TABLE x(a DEFAULT(randomblob(1)))",
    ] {
        ensure!(super::super::schema::validate(b"table", Some(sql.as_bytes())).is_err());
    }
    Ok(())
}
