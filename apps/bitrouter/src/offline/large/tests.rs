//! Bounded-engine regressions; adapter custody and complete flows have separate tests.
use super::*;
use crate::offline::{engine::Connections, files, inventory};
use anyhow::{Context, Result, ensure};
use sea_orm::sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::{io::Write, os::unix::fs::PermissionsExt, path::Path};
use tokio_util::sync::CancellationToken;

fn options(home: &Path, bytes: usize) -> Result<Options> {
    std::fs::set_permissions(home, std::fs::Permissions::from_mode(0o700))?;
    Ok(Options {
        working_bytes: bytes,
        scratch_directory: home.canonicalize()?,
        min_free_bytes: 1,
    })
}

#[test]
fn aggregate_reservations_cover_boundary_and_release() -> Result<()> {
    let home = tempfile::tempdir()?;
    let workspace = Workspace::create(
        &options(home.path(), 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let n = workspace.available();
    ensure!(
        std::fs::metadata(workspace.directory())?
            .permissions()
            .mode()
            & 0o777
            == 0o700
    );
    let first = workspace.reserve(n - 1)?;
    ensure!(workspace.available() == 1);
    ensure!(
        workspace
            .reserve(2)
            .err()
            .context("must refuse N+1")?
            .is::<CapacityExceeded>()
    );
    let last = workspace.reserve(1)?;
    ensure!(workspace.available() == 0);
    ensure!(workspace.reserve(1).is_err());
    drop(first);
    ensure!(workspace.available() == n - 1);
    drop(last);
    ensure!(workspace.available() == n);
    let all = workspace.reserve(n)?;
    drop(all);
    workspace.finish()?;
    ensure!(std::fs::read_dir(home.path())?.next().is_none());
    Ok(())
}

#[test]
fn completed_copy_and_spool_probe_failure_refuse_success_after_sync() -> Result<()> {
    let home = tempfile::tempdir()?;
    let workspace = Workspace::create(
        &options(home.path(), 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let source = home.path().join("source.sqlite");
    let copy = home.path().join("incomplete.sqlite");
    files::write_new(&source, b"retained bytes")?;
    workspace.refuse_completed_probe();
    let error = copy_new(&source, &copy, &workspace)
        .err()
        .context("post-sync copy refusal")?;
    ensure!(error.to_string().contains("completed-output probe"));
    ensure!(std::fs::read(&copy)? == std::fs::read(&source)?);
    let mut records = sort::Records::<4>::new(&workspace, 65536)?;
    records.push(1_u32.to_be_bytes())?;
    workspace.refuse_completed_probe();
    let error = records.finish().err().context("post-sync spool refusal")?;
    ensure!(error.to_string().contains("completed-output probe"));
    ensure!(std::fs::metadata(workspace.run_path(0, 0, 0))?.len() == 4);
    workspace.finish()?;
    Ok(())
}

#[tokio::test]
async fn sql_phase_keeps_allowance_until_real_worker_clear_ack() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("phase.sqlite");
    let mut seed = database(&path).await?;
    sea_orm::sqlx::query("CREATE TABLE samples(value INTEGER CHECK(value=1))")
        .execute(&mut seed)
        .await?;
    seed.close().await?;
    let workspace = Workspace::create(
        &options(home.path(), 16 * 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let baseline = workspace.available();
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.open_budgeted(&path, true, &workspace).await?;
    ensure!(workspace.available() == baseline - crate::offline::engine::CONNECTION_BYTES);
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
    let before = workspace.available();
    let phase = super::phase::Phase::new(&workspace, 1024 * 1024)?;
    let mut query = Box::pin(inventory::validate_checks_in(db, "samples"));
    let query_pending = futures::poll!(&mut query).is_pending();
    tokio::time::timeout(std::time::Duration::from_secs(10), entered.cancelled()).await?;
    drop(query);
    let mut clearing = Box::pin(phase.clear(db));
    let clear_pending = futures::poll!(&mut clearing).is_pending();
    let held = workspace.available() == before - 1024 * 1024;
    release
        .send(())
        .map_err(|_| anyhow::anyhow!("worker gate unavailable"))?;
    clearing.await?;
    ensure!(query_pending && clear_pending && held);
    ensure!(workspace.available() == before);
    connections.close().await?;
    ensure!(workspace.available() == baseline);
    workspace.finish()?;
    Ok(())
}

#[tokio::test]
async fn uncertain_close_keeps_connection_allowance_quarantined() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("uncertain.sqlite");
    database(&path).await?.close().await?;
    let workspace = Workspace::create(
        &options(home.path(), 16 * 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.open_budgeted(&path, true, &workspace).await?;
    let held = workspace.available();
    connections.uncertain = true;
    // Actual close is awaited, but an earlier unconfirmed outcome stays sticky.
    ensure!(connections.close().await.is_err());
    ensure!(workspace.available() == held);
    drop(connections);
    ensure!(workspace.available() == held);
    workspace.finish()?;
    Ok(())
}

#[tokio::test]
async fn utf16_inventory_matches_v1_and_cache_stays_charged_after_inspection() -> Result<()> {
    for encoding in ["UTF-16le", "UTF-16be"] {
        let home = tempfile::tempdir()?;
        let path = home.path().join("utf16.sqlite");
        let mut db = database(&path).await?;
        sea_orm::sqlx::query(&format!("PRAGMA encoding='{encoding}'"))
            .execute(&mut db)
            .await?;
        sea_orm::sqlx::query("CREATE TABLE samples(value)")
            .execute(&mut db)
            .await?;
        sea_orm::sqlx::query(
            "INSERT INTO samples VALUES('λ'),('same'),('same'),(X'0001'),(NULL),(19),(1.25)",
        )
        .execute(&mut db)
        .await?;
        db.close().await?;
        let workspace = Workspace::create(
            &options(home.path(), 64 * 1024 * 1024)?,
            CancellationToken::new(),
        )?;
        let before = workspace.available();
        let mut connections = Connections::new(CancellationToken::new(), None);
        connections.open_budgeted(&path, true, &workspace).await?;
        let after_open = workspace.available();
        let mut tx = connections
            .copy
            .as_mut()
            .context("copy missing")?
            .begin()
            .await?;
        let old = inventory::inspect_in(&mut tx, &[1; 32]).await?;
        let inspected = inspect_in(&mut tx, &[1; 32], &workspace).await?;
        ensure!(inspected.value() == &old && inspected.value().encoding == encoding);
        drop(inspected);
        ensure!(workspace.available() == after_open);
        tx.rollback().await?;
        connections.close().await?;
        ensure!(workspace.available() == before);
        workspace.finish()?;
    }
    Ok(())
}

#[tokio::test]
async fn oversized_cell_refuses_from_scalar_preflight_and_releases_acknowledged_phases()
-> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("oversized.sqlite");
    let mut db = database(&path).await?;
    sea_orm::sqlx::query("CREATE TABLE samples(value BLOB CHECK(value = 'allowed'))")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("PRAGMA ignore_check_constraints=ON")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO samples VALUES(zeroblob(?))")
        .bind(i64::try_from(crate::offline::format::MAX_CELL + 1)?)
        .execute(&mut db)
        .await?;
    db.close().await?;
    let workspace = Workspace::create(
        &options(home.path(), 16 * 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.open_budgeted(&path, true, &workspace).await?;
    let before = workspace.available();
    let mut tx = connections
        .copy
        .as_mut()
        .context("copy missing")?
        .begin()
        .await?;
    let error = inspect_in(&mut tx, &[1; 32], &workspace)
        .await
        .err()
        .context("oversized cell accepted")?;
    ensure!(
        error.to_string().contains("cell exceeds offline profile")
            && !error.is::<CapacityExceeded>()
    );
    ensure!(workspace.available() == before);
    tx.rollback().await?;
    connections.close().await?;
    workspace.finish()?;
    Ok(())
}

#[tokio::test]
async fn bounded_integrity_scalar_accepts_valid_and_rejects_retained_check_corruption() -> Result<()>
{
    check_inventory_case(
        "CREATE TABLE samples(value INTEGER CHECK(value IN (0,1)))",
        "INSERT INTO samples VALUES(1)",
        "INSERT INTO samples VALUES(-1)",
        1,
    )
    .await
}

async fn check_inventory_case(
    definition: &str,
    valid: &str,
    invalid: &str,
    valid_rows: u64,
) -> Result<()> {
    for corrupt in [false, true] {
        let home = tempfile::tempdir()?;
        let path = home.path().join("integrity.sqlite");
        let mut db = database(&path).await?;
        sea_orm::sqlx::query(definition).execute(&mut db).await?;
        sea_orm::sqlx::query(valid).execute(&mut db).await?;
        if corrupt {
            // SQLite's own writable CHECK enforcement establishes the oracle.
            let error = sea_orm::sqlx::query(invalid)
                .execute(&mut db)
                .await
                .err()
                .context("invalid fixture passed writable CHECK")?;
            ensure!(error.to_string().contains("CHECK constraint failed"));
            sea_orm::sqlx::query("PRAGMA ignore_check_constraints=ON")
                .execute(&mut db)
                .await?;
            sea_orm::sqlx::query(invalid).execute(&mut db).await?;
        }
        db.close().await?;
        let witness = Source::inspect(&path)?;
        let bytes = std::fs::read(&path)?;
        let workspace = Workspace::create(
            &options(home.path(), 32 * 1024 * 1024)?,
            CancellationToken::new(),
        )?;
        let baseline = workspace.available();
        let mut connections = Connections::new(CancellationToken::new(), None);
        connections.open_budgeted(&path, true, &workspace).await?;
        let held = workspace.available();
        let mut tx = connections
            .copy
            .as_mut()
            .context("copy missing")?
            .begin()
            .await?;
        ensure!(sea_orm::sqlx::query(valid).execute(&mut *tx).await.is_err());
        let legacy = inventory::inspect_in(&mut tx, &[9; 32]).await;
        let actual = inspect_in(&mut tx, &[9; 32], &workspace).await;
        if corrupt {
            let error = actual.err().context("corrupt database accepted")?;
            ensure!(error.to_string() == "SQLite integrity failed");
            ensure!(
                legacy
                    .err()
                    .context("v1 accepted corrupt CHECK")?
                    .to_string()
                    == "SQLite integrity failed"
            );
        } else {
            let actual = actual?;
            ensure!(actual.value().rows == valid_rows && actual.value() == &legacy?);
        }
        // Both outcomes cross ClearCache ACK; only the connection remains held.
        ensure!(workspace.available() == held);
        tx.rollback().await?;
        connections.close().await?;
        ensure!(workspace.available() == baseline);
        witness.check()?;
        ensure!(std::fs::read(&path)? == bytes);
        workspace.finish()?;
    }
    Ok(())
}

#[tokio::test]
async fn readonly_checks_preserve_sqlite_null_affinity_collation_and_names() -> Result<()> {
    for (definition, valid, invalid, rows) in [
        (
            "CREATE TABLE samples(value INTEGER CHECK(value != 0) CHECK(value BETWEEN -2 AND +2))",
            "INSERT INTO samples VALUES(1)",
            "INSERT INTO samples VALUES(3)",
            1,
        ),
        (
            "CREATE TABLE \"sam\"\"ples\"(\"va\"\"lue\" TEXT COLLATE NOCASE CHECK(\"va\"\"lue\" = 'o''hare') CHECK(\"va\"\"lue\" <> 'forbidden'))",
            "INSERT INTO \"sam\"\"ples\" VALUES('O''HARE')",
            "INSERT INTO \"sam\"\"ples\" VALUES('other')",
            1,
        ),
        (
            "CREATE TABLE samples(value NUMERIC CHECK(value = '01'))",
            "INSERT INTO samples VALUES(1)",
            "INSERT INTO samples VALUES(2)",
            1,
        ),
        (
            "CREATE TABLE samples(value INTEGER CHECK(value = 9007199254740993))",
            "INSERT INTO samples VALUES(9007199254740993)",
            "INSERT INTO samples VALUES(9007199254740992)",
            1,
        ),
        (
            "CREATE TABLE samples(value INTEGER CHECK(value IN(TRUE,FALSE)))",
            "INSERT INTO samples VALUES(NULL),(0),(1)",
            "INSERT INTO samples VALUES(2)",
            3,
        ),
        (
            "CREATE TABLE samples(value INTEGER,\"true\" INTEGER,\"false\" INTEGER,CHECK(value IN(TRUE,FALSE)))",
            "INSERT INTO samples VALUES(7,7,9),(NULL,7,9)",
            "INSERT INTO samples VALUES(1,7,9)",
            2,
        ),
        (
            "CREATE TABLE samples(value TEXT CHECK(\"missing\" = 'missing'), CHECK(rowid != 0))",
            "INSERT INTO samples(rowid,value) VALUES(5,'retained')",
            "INSERT INTO samples(rowid,value) VALUES(0,'invalid')",
            1,
        ),
    ] {
        check_inventory_case(definition, valid, invalid, rows).await?;
    }
    Ok(())
}

#[tokio::test]
async fn readonly_check_operand_budget_refuses_before_corruption_and_releases_phases() -> Result<()>
{
    let home = tempfile::tempdir()?;
    let path = home.path().join("check-budget.sqlite");
    let mut db = database(&path).await?;
    sea_orm::sqlx::query("CREATE TABLE samples(value BLOB CHECK(value = 'allowed'))")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("PRAGMA ignore_check_constraints=ON")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO samples VALUES(zeroblob(2097152))")
        .execute(&mut db)
        .await?;
    db.close().await?;
    for mib in [16, 64] {
        let workspace = Workspace::create(
            &options(home.path(), mib * 1024 * 1024)?,
            CancellationToken::new(),
        )?;
        let baseline = workspace.available();
        let mut connections = Connections::new(CancellationToken::new(), None);
        connections.open_budgeted(&path, true, &workspace).await?;
        let held = workspace.available();
        let mut tx = connections
            .copy
            .as_mut()
            .context("copy missing")?
            .begin()
            .await?;
        let error = inspect_in(&mut tx, &[1; 32], &workspace)
            .await
            .err()
            .context("invalid CHECK accepted")?;
        if mib == 16 {
            ensure!(error.is::<CapacityExceeded>());
        } else {
            ensure!(error.to_string() == "SQLite integrity failed");
        }
        ensure!(workspace.available() == held);
        tx.rollback().await?;
        connections.close().await?;
        ensure!(workspace.available() == baseline);
        workspace.finish()?;
    }
    Ok(())
}

#[tokio::test]
async fn unsafe_check_schema_refuses_before_an_earlier_violating_predicate() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("unsafe-check.sqlite");
    let mut db = database(&path).await?;
    sea_orm::sqlx::query("CREATE TABLE samples(value INTEGER CHECK(value=1), CHECK(value=abs(1)))")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("PRAGMA ignore_check_constraints=ON")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO samples VALUES(2)")
        .execute(&mut db)
        .await?;
    db.close().await?;
    let workspace = Workspace::create(
        &options(home.path(), 32 * 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.open_budgeted(&path, true, &workspace).await?;
    let held = workspace.available();
    let mut tx = connections
        .copy
        .as_mut()
        .context("copy missing")?
        .begin()
        .await?;
    let legacy = inventory::inspect_in(&mut tx, &[1; 32])
        .await
        .err()
        .context("v1 accepted executable CHECK")?;
    let large = inspect_in(&mut tx, &[1; 32], &workspace)
        .await
        .err()
        .context("v2 accepted executable CHECK")?;
    ensure!(legacy.to_string() == "unsupported schema literal");
    ensure!(large.to_string() == legacy.to_string());
    ensure!(workspace.available() == held);
    tx.rollback().await?;
    connections.close().await?;
    workspace.finish()?;
    Ok(())
}

#[test]
fn external_sort_preserves_duplicates_across_odd_runs_and_empty_input() -> Result<()> {
    let home = tempfile::tempdir()?;
    let workspace = Workspace::create(
        &options(home.path(), 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    // Three records per run, five runs, including repeated values across runs.
    let memory = 4 * 8192 + 6 * 4 + 3 * 4;
    let mut records = sort::Records::<4>::new(&workspace, memory)?;
    let input = [7_u32, 0, 7, 9, 1, 1, 3, 7, 2, 9, 0, 4, 7];
    for value in input {
        records.push(value.to_be_bytes())?;
    }
    let mut sorted = records.finish()?;
    ensure!(sorted.len() == input.len() as u64);
    let mut expected = input;
    expected.sort_unstable();
    ensure!(sorted.read_at(5)? == Some(expected[5].to_be_bytes()));
    ensure!(sorted.read_at(sorted.len())?.is_none());
    for value in expected {
        ensure!(sorted.next()? == Some(value.to_be_bytes()));
    }
    ensure!(sorted.next()?.is_none());
    drop(sorted);
    let mut empty = sort::Records::<4>::new(&workspace, memory)?.finish()?;
    ensure!(empty.len() == 0 && empty.next()?.is_none());
    drop(empty);
    workspace.finish()?;
    ensure!(std::fs::read_dir(home.path())?.next().is_none());
    Ok(())
}

#[test]
fn truncated_spool_and_cancellation_refuse_and_scratch_is_removed() -> Result<()> {
    let home = tempfile::tempdir()?;
    let cancel = CancellationToken::new();
    let workspace = Workspace::create(&options(home.path(), 1024 * 1024)?, cancel.clone())?;
    let memory = 4 * 8192 + 6 * 4 + 3 * 4;
    let mut records = sort::Records::<4>::new(&workspace, memory)?;
    for value in 0_u32..4 {
        records.push(value.to_be_bytes())?;
    }
    std::fs::OpenOptions::new()
        .write(true)
        .open(workspace.run_path(0, 0, 0))?
        .set_len(11)?;
    ensure!(records.finish().is_err());
    let mut records = sort::Records::<4>::new(&workspace, memory)?;
    for value in 0_u32..4 {
        records.push(value.to_be_bytes())?;
    }
    cancel.cancel();
    ensure!(records.push(5_u32.to_be_bytes()).is_err());
    drop(records);
    workspace.finish()?;
    ensure!(std::fs::read_dir(home.path())?.next().is_none());
    Ok(())
}

#[test]
fn failed_spool_create_and_space_probe_never_replace_files() -> Result<()> {
    let home = tempfile::tempdir()?;
    let workspace = Workspace::create(
        &options(home.path(), 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let path = workspace.run_path(0, 0, 0);
    files::write_new(&path, b"existing")?;
    let records = sort::Records::<4>::new(&workspace, 65536)?;
    ensure!(records.finish().is_err());
    ensure!(std::fs::read(&path)? == b"existing");
    ensure!(workspace.probe(home.path(), u64::MAX).is_err());
    workspace.finish()?;
    Ok(())
}

#[test]
fn file_digest_matches_v1_and_source_refuses_sidecars_and_replacement() -> Result<()> {
    let home = tempfile::tempdir()?;
    let workspace = Workspace::create(
        &options(home.path(), 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let source = home.path().join("source.sqlite");
    files::write_new(&source, b"same bytes, same domain")?;
    ensure!(
        snapshot_digest(&source, &[7; 32], &workspace)?
            == files::snapshot_digest(&source, &[7; 32])?
    );
    let witness = Source::inspect(&source)?;
    let copy = home.path().join("copy.sqlite");
    ensure!(copy_new(&source, &copy, &workspace)? == 23);
    ensure!(copy_new(&source, &copy, &workspace).is_err());
    files::write_new(&home.path().join("source.sqlite-wal"), b"retained")?;
    ensure!(witness.check().is_err());
    ensure!(Source::inspect(&source).is_err());
    ensure!(snapshot_digest(&source, &[7; 32], &workspace).is_err());
    ensure!(std::fs::read(home.path().join("source.sqlite-wal"))? == b"retained");
    std::fs::remove_file(home.path().join("source.sqlite-wal"))?;
    std::fs::OpenOptions::new()
        .append(true)
        .open(&source)?
        .write_all(b"changed")?;
    ensure!(witness.check().is_err());
    workspace.finish()?;
    Ok(())
}

#[test]
fn opaque_copy_accepts_adjacent_wal_named_selected_files() -> Result<()> {
    let home = tempfile::tempdir()?;
    let workspace = Workspace::create(
        &options(home.path(), 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let first = home.path().join("x");
    let second = home.path().join("x-wal");
    files::write_new(&first, b"opaque first")?;
    files::write_new(&second, b"opaque second")?;
    let first_copy = home.path().join("copied-x");
    let second_copy = home.path().join("copied-x-wal");
    ensure!(copy_new(&first, &first_copy, &workspace)? == 12);
    ensure!(copy_new(&second, &second_copy, &workspace)? == 13);
    ensure!(std::fs::read(&first_copy)? == std::fs::read(&first)?);
    ensure!(std::fs::read(&second_copy)? == std::fs::read(&second)?);
    workspace.finish()?;
    Ok(())
}

#[tokio::test]
async fn database_snapshot_still_refuses_adjacent_sqlite_sidecars() -> Result<()> {
    let home = tempfile::tempdir()?;
    let workspace = Workspace::create(
        &options(home.path(), 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let source = home.path().join("snapshot.sqlite");
    let mut db = database(&source).await?;
    sea_orm::sqlx::query("CREATE TABLE retained(value)")
        .execute(&mut db)
        .await?;
    db.close().await?;
    let witness = Source::inspect(&source)?;
    files::write_new(
        &home.path().join("snapshot.sqlite-wal"),
        b"retained sidecar",
    )?;
    ensure!(witness.check().is_err());
    ensure!(Source::inspect(&source).is_err());
    ensure!(snapshot_digest(&source, &[7; 32], &workspace).is_err());
    ensure!(std::fs::read(home.path().join("snapshot.sqlite-wal"))? == b"retained sidecar");
    workspace.finish()?;
    Ok(())
}

async fn database(path: &Path) -> Result<SqliteConnection> {
    files::write_new(path, &[])?;
    Ok(SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .row_buffer_size(1)
            .statement_cache_capacity(0)
            .pragma("cache_size", "-2048"),
    )
    .await?)
}

#[tokio::test]
async fn complete_inventory_matches_v1_typed_values_and_empty_tables() -> Result<()> {
    let home = tempfile::tempdir()?;
    let workspace = Workspace::create(
        &options(home.path(), 64 * 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let path = home.path().join("values.sqlite");
    let mut db = database(&path).await?;
    sea_orm::sqlx::query("CREATE TABLE samples(value)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("CREATE TABLE empty(value BLOB)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO samples VALUES(NULL),(''),(X''),(CAST(X'FF00' AS TEXT)),(X'FF00'),(17),(17.25),('duplicate'),('duplicate')")
        .execute(&mut db).await?;
    db.close().await?;
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.open_budgeted(&path, true, &workspace).await?;
    let mut tx = connections
        .copy
        .as_mut()
        .context("copy missing")?
        .begin()
        .await?;
    let legacy = inventory::inspect_in(&mut tx, &[3; 32]).await?;
    let large = inspect_in(&mut tx, &[3; 32], &workspace).await?;
    ensure!(*large.value() == legacy);
    ensure!(large.value().rows == 9);
    tx.rollback().await?;
    drop(large);
    connections.close().await?;
    workspace.finish()?;
    Ok(())
}

#[tokio::test]
async fn legal_nine_mib_row_fits_default_and_low_working_bytes_is_typed() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("body.sqlite");
    let mut db = database(&path).await?;
    sea_orm::sqlx::query("CREATE TABLE bodies(document BLOB,envelope BLOB)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO bodies VALUES(zeroblob(1048576),zeroblob(8388608))")
        .execute(&mut db)
        .await?;
    db.close().await?;
    let small = Workspace::create(
        &options(home.path(), 16 * 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.open_budgeted(&path, true, &small).await?;
    let mut tx = connections
        .copy
        .as_mut()
        .context("copy missing")?
        .begin()
        .await?;
    let error = inspect_in(&mut tx, &[3; 32], &small)
        .await
        .err()
        .context("must refuse body allocation")?;
    ensure!(error.is::<CapacityExceeded>());
    tx.rollback().await?;
    connections.close().await?;
    small.finish()?;
    let large = Workspace::create(
        &options(home.path(), 64 * 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    connections.open_budgeted(&path, true, &large).await?;
    let mut tx = connections
        .copy
        .as_mut()
        .context("copy missing")?
        .begin()
        .await?;
    let inspected = inspect_in(&mut tx, &[3; 32], &large).await?;
    ensure!(inspected.value().rows == 1 && inspected.value().value_bytes == 9 * 1024 * 1024);
    drop(inspected);
    tx.rollback().await?;
    connections.close().await?;
    large.finish()?;
    Ok(())
}

#[tokio::test]
async fn clean_wal_close_immutable_source_uses_source_slot_and_creates_no_sidecars() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("wal.sqlite");
    let mut db = database(&path).await?;
    sea_orm::sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("CREATE TABLE samples(value)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO samples VALUES('retained')")
        .execute(&mut db)
        .await?;
    ensure!(Source::inspect(&path).is_err());
    db.close().await?;
    let owner = crate::tenancy::ownership::acquire_path(&path, false)?;
    let path = owner.path.clone();
    let witness = Source::inspect(&path)?;
    let mut connections = Connections::new(CancellationToken::new(), Some(owner));
    connections.validation_budget().await?;
    connections.open_immutable_source(&path).await?;
    ensure!(connections.source.is_some() && connections.copy.is_none());
    ensure!(connections.open_immutable_source(&path).await.is_err());
    witness.check()?;
    let mut tx = connections
        .source
        .as_mut()
        .context("source missing")?
        .begin()
        .await?;
    let value: String = sea_orm::sqlx::query_scalar("SELECT value FROM samples")
        .fetch_one(&mut *tx)
        .await?;
    ensure!(value == "retained");
    tx.rollback().await?;
    connections.close().await?;
    witness.check()?;
    Ok(())
}

#[tokio::test]
#[ignore = "complete dense large-data regression; run explicitly with --ignored"]
async fn dense_inventory_exceeds_all_v1_total_limits_with_fixed_working_budget() -> Result<()> {
    let home = tempfile::tempdir()?;
    let path = home.path().join("dense.sqlite");
    let mut db = database(&path).await?;
    sea_orm::sqlx::query("CREATE TABLE samples(value BLOB)")
        .execute(&mut db)
        .await?;
    // Over 512 MiB of real typed row content, over one million physical rows.
    sea_orm::sqlx::query("WITH RECURSIVE n(x) AS (SELECT 1 UNION ALL SELECT x+1 FROM n WHERE x<1000001) INSERT INTO samples SELECT zeroblob(544) FROM n")
        .execute(&mut db).await?;
    db.close().await?;
    let workspace = Workspace::create(
        &options(home.path(), 32 * 1024 * 1024)?,
        CancellationToken::new(),
    )?;
    let mut connections = Connections::new(CancellationToken::new(), None);
    connections.validation_budget().await?;
    connections.open_budgeted(&path, true, &workspace).await?;
    let mut tx = connections
        .copy
        .as_mut()
        .context("copy missing")?
        .begin()
        .await?;
    ensure!(inventory::inspect_in(&mut tx, &[5; 32]).await.is_err());
    let actual = inspect_in(&mut tx, &[5; 32], &workspace).await?;
    ensure!(actual.value().rows == 1_000_001);
    ensure!(actual.value().value_bytes > crate::offline::format::MAX_FILE);
    drop(actual);
    tx.rollback().await?;
    connections.close().await?;
    ensure!(std::fs::metadata(&path)?.len() > crate::offline::format::MAX_FILE);
    let copy = home.path().join("copy.sqlite");
    copy_new(&path, &copy, &workspace)?;
    ensure!(
        snapshot_digest(&path, &[5; 32], &workspace)?
            == snapshot_digest(&copy, &[5; 32], &workspace)?
    );
    workspace.finish()?;
    Ok(())
}
