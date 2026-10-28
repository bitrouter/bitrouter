use super::*;
use anyhow::{Result, ensure};
use sea_orm::sqlx::{
    Connection, Row, SqlitePool,
    sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};

async fn inspect(db: &SqlitePool, key: &[u8; 32]) -> Result<format::Inventory> {
    let mut connection = db.acquire().await?;
    inventory::inspect(&mut connection, key).await
}

async fn fixture(path: &Path) -> Result<SqlitePool> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .create_if_missing(true);
    let pool = SqlitePoolOptions::new()
        .max_connections(1)
        .connect_with(options)
        .await?;
    sea_orm::sqlx::query("CREATE TABLE samples(id INTEGER PRIMARY KEY,value)")
        .execute(&pool)
        .await?;
    sea_orm::sqlx::query(
        "INSERT INTO samples VALUES(1,'PRIVATE_PAYLOAD'),(2,X'00FF'),(3,NULL),(4,17),(5,17.25)",
    )
    .execute(&pool)
    .await?;
    sea_orm::sqlx::query(
        "CREATE TABLE seaql_migrations(version TEXT PRIMARY KEY,applied_at BIGINT NOT NULL)",
    )
    .execute(&pool)
    .await?;
    sea_orm::sqlx::query(
        "INSERT INTO seaql_migrations VALUES('m20240101_000007_historical',123456)",
    )
    .execute(&pool)
    .await?;
    Ok(pool)
}

#[tokio::test]
async fn source_bytes_ledger_and_unknown_provenance_survive_real_snapshot() -> Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("source.sqlite");
    let db = fixture(&source).await?;
    db.close().await;
    let old = std::fs::read(&source)?;
    let bundle = home.path().join("bundle");
    let report = snapshot_inner(&source, &bundle).await?;
    ensure!(std::fs::read(&source)? == old);
    ensure!(report.evidence.source_commit.is_none() && report.evidence.source_build.is_none());
    ensure!(report.evidence.inventory.installation_id.is_none());
    let ledger = report
        .evidence
        .inventory
        .ledger
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("ledger missing"))?;
    ensure!(ledger.len() == 1 && ledger[0].applied_at == 123456);
    ensure!(report.evidence.inventory.rows == 6);
    ensure!(!serde_json::to_string(&report)?.contains("PRIVATE_PAYLOAD"));
    let verified = read_bundle(&bundle, Some(&report.evidence_digest), "verify").await?;
    ensure!(
        verified.evidence_anchor_checked
            && !verified.evidence.complete
            && !verified.evidence.cutover_ready
    );
    ensure!(snapshot_inner(&source, &bundle).await.is_err());
    ensure!(std::fs::read(&source)? == old);
    Ok(())
}

#[tokio::test]
async fn typed_values_order_and_duplicate_rows_have_exact_bounded_digests() -> Result<()> {
    let home = tempfile::tempdir()?;
    let db = fixture(&home.path().join("values.sqlite")).await?;
    sea_orm::sqlx::query("CREATE TABLE duplicates(value)")
        .execute(&db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO duplicates VALUES(CAST(X'FF00' AS TEXT)),(X'FF00'),(NULL),(''),(X''),('same'),('same')").execute(&db).await?;
    let first = inspect(&db, &[7; 32]).await?;
    let table = first
        .tables
        .iter()
        .find(|t| t.name == "duplicates")
        .ok_or_else(|| anyhow::anyhow!("missing table"))?;
    ensure!(table.rows == 7);
    let digest = table.digest.clone();
    sea_orm::sqlx::query("DELETE FROM duplicates WHERE rowid=(SELECT max(rowid) FROM duplicates)")
        .execute(&db)
        .await?;
    let second = inspect(&db, &[7; 32]).await?;
    ensure!(
        second
            .tables
            .iter()
            .any(|t| t.name == "duplicates" && t.rows == 6 && t.digest != digest)
    );
    sea_orm::sqlx::query("INSERT INTO duplicates VALUES('same')")
        .execute(&db)
        .await?;
    ensure!(inspect(&db, &[7; 32]).await? == first);
    sea_orm::sqlx::query(
        "UPDATE duplicates SET value=CAST(value AS BLOB) WHERE typeof(value)='text'",
    )
    .execute(&db)
    .await?;
    ensure!(inspect(&db, &[7; 32]).await? != first);
    db.close().await;
    Ok(())
}

#[tokio::test]
async fn oversized_cell_and_generated_columns_fail_without_publishing_ready() -> Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("large.sqlite");
    let db = fixture(&source).await?;
    sea_orm::sqlx::query("INSERT INTO samples VALUES(6,zeroblob(?))")
        .bind((format::MAX_CELL + 1) as i64)
        .execute(&db)
        .await?;
    db.close().await;
    let bundle = home.path().join("large-bundle");
    ensure!(snapshot(&source, &bundle).await.is_err());
    ensure!(bundle.join("INCOMPLETE").exists() && !bundle.join("READY").exists());
    ensure!(inventory(&bundle).await.is_err());
    let source = home.path().join("generated.sqlite");
    let db = fixture(&source).await?;
    sea_orm::sqlx::query("CREATE TABLE generated(a INT,b INT GENERATED ALWAYS AS(a+1))")
        .execute(&db)
        .await?;
    ensure!(inspect(&db, &[8; 32]).await.is_err());
    db.close().await;
    Ok(())
}

#[tokio::test]
async fn canonical_writer_custody_blocks_alias_and_rejects_hardlink() -> Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("source.sqlite");
    fixture(&source).await?.close().await;
    let alias = home.path().join("alias.sqlite");
    std::os::unix::fs::symlink(&source, &alias)?;
    let writer = ownership::acquire_writer(&source, false)?;
    let bundle = home.path().join("busy");
    ensure!(snapshot(&alias, &bundle).await.is_err() && !bundle.exists());
    drop(writer);
    let hard = home.path().join("hard.sqlite");
    std::fs::hard_link(&source, &hard)?;
    ensure!(snapshot(&hard, &bundle).await.is_err() && !bundle.exists());
    std::fs::remove_file(hard)?;
    snapshot(&alias, &bundle).await?;
    Ok(())
}

#[tokio::test]
async fn absent_ledger_is_not_reported_as_empty_and_installation_is_never_created() -> Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("legacy.sqlite");
    let db = fixture(&source).await?;
    sea_orm::sqlx::query("DROP TABLE seaql_migrations")
        .execute(&db)
        .await?;
    db.close().await;
    let bundle = home.path().join("bundle");
    let report = snapshot_inner(&source, &bundle).await?;
    ensure!(report.evidence.inventory.ledger.is_none());
    ensure!(report.evidence.inventory.installation_id.is_none());
    let mut db = sea_orm::sqlx::SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&source)
            .read_only(true),
    )
    .await?;
    let row=sea_orm::sqlx::query("SELECT count(*) AS count FROM sqlite_schema WHERE name LIKE 'tenancy_%' OR name='seaql_migrations'").fetch_one(&mut db).await?;
    ensure!(row.try_get::<i64, _>("count")? == 0);
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn one_row_cannot_multiply_the_cell_allowance_or_lose_installation_metadata() -> Result<()> {
    let home = tempfile::tempdir()?;
    let db = fixture(&home.path().join("rows.sqlite")).await?;
    sea_orm::sqlx::query(
        "CREATE TABLE tenancy_installation(singleton INT PRIMARY KEY,installation_id TEXT)",
    )
    .execute(&db)
    .await?;
    sea_orm::sqlx::query("INSERT INTO tenancy_installation VALUES(1,'actual-installation')")
        .execute(&db)
        .await?;
    ensure!(
        inspect(&db, &[4; 32]).await?.installation_id.as_deref() == Some("actual-installation")
    );
    sea_orm::sqlx::query("CREATE TABLE large_row(id INT PRIMARY KEY,a BLOB,b BLOB)")
        .execute(&db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO large_row VALUES(1,zeroblob(?),zeroblob(?))")
        .bind(format::MAX_CELL as i64)
        .bind(format::MAX_CELL as i64)
        .execute(&db)
        .await?;
    ensure!(inspect(&db, &[4; 32]).await.is_err());
    db.close().await;
    Ok(())
}

#[tokio::test]
async fn wal_values_are_read_from_source_without_immutable_shortcut() -> Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("wal.sqlite");
    let db = fixture(&source).await?;
    sea_orm::sqlx::query("PRAGMA journal_mode=WAL")
        .execute(&db)
        .await?;
    sea_orm::sqlx::query("PRAGMA wal_autocheckpoint=0")
        .execute(&db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO samples VALUES(6,'UNPRINTED_WAL_VALUE')")
        .execute(&db)
        .await?;
    // This raw fixture is quiescent but deliberately keeps its WAL open. It is
    // not evidence that the sidecar can fence a nonparticipating old writer.
    let bundle = home.path().join("bundle");
    let result = snapshot_inner(&source, &bundle).await;
    db.close().await;
    let report = result?;
    ensure!(report.evidence.inventory.rows == 7);
    ensure!(report.evidence.inspector == crate::artifact::current());
    ensure!(!serde_json::to_string(&report)?.contains("UNPRINTED_WAL_VALUE"));
    read_bundle(&bundle, Some(&report.evidence_digest), "verify").await?;
    Ok(())
}

#[tokio::test]
async fn metadata_queries_project_invalid_types_to_null_before_transfer() -> Result<()> {
    use sea_orm::sqlx::ValueRef;
    let home = tempfile::tempdir()?;
    let source = home.path().join("metadata.sqlite");
    let db = fixture(&source).await?;
    sea_orm::sqlx::query("CREATE TABLE tenancy_installation(singleton,installation_id)")
        .execute(&db)
        .await?;
    for cast in ["TEXT", "BLOB"] {
        sea_orm::sqlx::query("DELETE FROM seaql_migrations")
            .execute(&db)
            .await?;
        sea_orm::sqlx::query("DELETE FROM tenancy_installation")
            .execute(&db)
            .await?;
        // Tiny deterministic cells within the generic row budget. Projection,
        // not eventual decoding/error, must prevent the metadata transfer.
        let sql = format!(
            "INSERT INTO seaql_migrations VALUES('m20240101_000007_historical',CAST(? AS {cast}))"
        );
        sea_orm::sqlx::query(&sql)
            .bind(vec![b'x'; 4096])
            .execute(&db)
            .await?;
        let sql = format!(
            "INSERT INTO tenancy_installation VALUES(CAST(? AS {cast}),'actual-installation')"
        );
        sea_orm::sqlx::query(&sql)
            .bind(vec![b'x'; 4096])
            .execute(&db)
            .await?;
        let ledger = sea_orm::sqlx::query(inventory::LEDGER_SQL)
            .fetch_one(&db)
            .await?;
        ensure!(ledger.try_get_raw("applied_at")?.is_null());
        ensure!(ledger.try_get::<String, _>("kind")? == cast.to_ascii_lowercase());
        let installation = sea_orm::sqlx::query(inventory::INSTALLATION_SQL)
            .fetch_one(&db)
            .await?;
        ensure!(installation.try_get_raw("singleton")?.is_null());
        ensure!(inspect(&db, &[2; 32]).await.is_err());
    }
    sea_orm::sqlx::query("DELETE FROM seaql_migrations")
        .execute(&db)
        .await?;
    sea_orm::sqlx::query(
        "INSERT INTO seaql_migrations VALUES(CAST('m20240101_000007_historical' AS BLOB),7)",
    )
    .execute(&db)
    .await?;
    sea_orm::sqlx::query("DELETE FROM tenancy_installation")
        .execute(&db)
        .await?;
    sea_orm::sqlx::query(
        "INSERT INTO tenancy_installation VALUES(1,CAST('actual-installation' AS BLOB))",
    )
    .execute(&db)
    .await?;
    ensure!(
        sea_orm::sqlx::query(inventory::LEDGER_SQL)
            .fetch_one(&db)
            .await?
            .try_get_raw("version")?
            .is_null()
    );
    ensure!(
        sea_orm::sqlx::query(inventory::INSTALLATION_SQL)
            .fetch_one(&db)
            .await?
            .try_get_raw("installation_id")?
            .is_null()
    );
    db.close().await;
    let bundle = home.path().join("invalid");
    ensure!(snapshot(&source, &bundle).await.is_err());
    ensure!(bundle.join("INCOMPLETE").exists() && !bundle.join("READY").exists());
    Ok(())
}

#[tokio::test]
async fn actual_migrated_bitrouter_schema_with_historical_checks_can_snapshot() -> Result<()> {
    let home = tempfile::tempdir()?;
    let source = home.path().join("actual.sqlite");
    let url = format!("sqlite://{}?mode=rwc", source.display());
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    db.close().await?;
    // The real pool's ownership hook may finish dropping after close returns.
    // Observe that lifecycle boundary before opening any raw fixture descriptor.
    let owner = tokio::time::timeout(std::time::Duration::from_secs(10), async {
        loop {
            if let Ok(owner) = ownership::acquire_path(&source, false) {
                break owner;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;
    let bytes = std::fs::read(&source)?;
    drop(owner);
    let bundle = home.path().join("actual-bundle");
    let report = snapshot_inner(&source, &bundle).await?;
    ensure!(std::fs::read(&source)? == bytes);
    let ledger = report
        .evidence
        .inventory
        .ledger
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("actual ledger absent"))?;
    use sea_orm_migration::MigratorTrait;
    let expected: Vec<_> = crate::db::migration::Migrator::migrations()
        .into_iter()
        .map(|m| m.name().to_owned())
        .collect();
    ensure!(ledger.iter().map(|m| &m.version).eq(expected.iter()));
    ensure!(
        report
            .evidence
            .inventory
            .tables
            .iter()
            .any(|t| t.name == "provider_continuations")
    );
    let verified = read_bundle(&bundle, Some(&report.evidence_digest), "verify").await?;
    ensure!(verified.evidence.inventory == report.evidence.inventory);
    ensure!(!verified.evidence.complete && !verified.evidence.cutover_ready);
    Ok(())
}

#[tokio::test]
async fn tiny_expression_index_and_generated_column_never_reach_snapshot_copy() -> Result<()> {
    for suffix in ["expression", "partial", "generated"] {
        let home = tempfile::tempdir()?;
        let source = home.path().join("source.sqlite");
        let db = fixture(&source).await?;
        let sql = match suffix {
            "expression" => "CREATE INDEX executable ON samples(length(value))",
            "partial" => "CREATE INDEX executable ON samples(id) WHERE length(value)>0",
            _ => "CREATE TABLE generated(a INT,b INT GENERATED ALWAYS AS(a+1))",
        };
        sea_orm::sqlx::query(sql).execute(&db).await?;
        db.close().await;
        let bundle = home.path().join(suffix);
        ensure!(snapshot(&source, &bundle).await.is_err());
        ensure!(std::fs::metadata(bundle.join("snapshot.sqlite"))?.len() == 0);
        ensure!(bundle.join("INCOMPLETE").exists() && !bundle.join("READY").exists());
    }
    Ok(())
}
