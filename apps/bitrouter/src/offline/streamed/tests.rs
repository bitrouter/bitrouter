use super::*;
use sea_orm::sqlx::{Connection, SqliteConnection, sqlite::SqliteConnectOptions};
use std::os::unix::fs::PermissionsExt;

fn options(home: &Path) -> Options {
    Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.into(),
        min_free_bytes: 1024 * 1024,
    }
}
async fn fixture(path: &Path) -> Result<()> {
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(path)
            .create_if_missing(true),
    )
    .await?;
    sea_orm::sqlx::query("CREATE TABLE retained(value)")
        .execute(&mut db)
        .await?;
    sea_orm::sqlx::query("INSERT INTO retained VALUES ('secret-payload'),('same'),('same'),(NULL),(X'FF00'),(17),(17.25)").execute(&mut db).await?;
    db.close().await?;
    std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    Ok(())
}
async fn take(source: &Path, bundle: &Path, options: Options) -> Result<Report> {
    run(
        Action::Snapshot {
            database: source.into(),
            bundle: bundle.into(),
        },
        options,
    )
    .await
}

#[tokio::test]
async fn complete_evidence_verifies_database_and_independent_digest() -> Result<()> {
    let home = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let source = home.path().join("source.sqlite");
    let bundle = home.path().join("bundle");
    fixture(&source).await?;
    let original = std::fs::read(&source)?;
    let report = take(&source, &bundle, options(home.path())).await?;
    assert_eq!(report.evidence.format, FORMAT_V2);
    assert_eq!(report.evidence.inventory.rows, 7);
    assert!(!report.evidence.complete);
    assert!(!serde_json::to_string(&report)?.contains("secret-payload"));
    assert_eq!(std::fs::read(&source)?, original);
    assert_eq!(std::fs::read(bundle.join("snapshot.sqlite"))?, original);
    assert!(!bundle.join("INCOMPLETE").exists());
    let key: [u8; 32] = files::read(&bundle.join("inventory.key"), 32)?
        .try_into()
        .map_err(|_| anyhow::anyhow!("key shape"))?;
    let mut db = SqliteConnection::connect_with(
        &SqliteConnectOptions::new()
            .filename(&source)
            .read_only(true)
            .immutable(true),
    )
    .await?;
    let old = crate::offline::inventory::inspect(&mut db, &key).await?;
    db.close().await?;
    assert_eq!(old, report.evidence.inventory);
    assert_eq!(
        files::snapshot_digest(&source, &key)?,
        (
            report.evidence.snapshot_bytes,
            report.evidence.snapshot_digest.clone()
        )
    );
    let verify = |expected: String| {
        run(
            Action::Read {
                bundle: bundle.clone(),
                expected: Some(expected),
                action: "verify",
            },
            options(home.path()),
        )
    };
    let verified = verify(report.evidence_digest.clone()).await?;
    assert!(verified.evidence_anchor_checked && verified.database_inventory_verified);
    assert_eq!(verified.evidence.inventory, old);
    let wrong = "0".repeat(64);
    assert!(verify(wrong.clone()).await.is_err());
    assert!(take(&source, &bundle, options(home.path())).await.is_err());
    std::fs::write(bundle.join("READY"), format!("{wrong}\n"))?;
    assert!(verify(report.evidence_digest).await.is_err());
    Ok(())
}

#[tokio::test]
async fn stopped_source_and_working_capacity_refusals_preserve_bytes_and_never_publish()
-> Result<()> {
    let home = tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?;
    let source = home.path().join("source.sqlite");
    fixture(&source).await?;
    let original = std::fs::read(&source)?;
    for suffix in ["-wal", "-shm", "-journal"] {
        let sidecar = home.path().join(format!("source.sqlite{suffix}"));
        std::fs::write(&sidecar, b"retained unclassified journal")?;
        let bundle = home.path().join(format!("refused{suffix}"));
        assert!(take(&source, &bundle, options(home.path())).await.is_err());
        assert!(!bundle.exists());
        assert_eq!(std::fs::read(&sidecar)?, b"retained unclassified journal");
        std::fs::remove_file(sidecar)?;
    }
    let mut small = options(home.path());
    small.working_bytes = 1;
    let bundle = home.path().join("too-small");
    assert!(take(&source, &bundle, small).await.is_err());
    assert!(!bundle.exists());
    let mut full = options(home.path());
    full.min_free_bytes = u64::MAX;
    assert!(
        take(&source, &home.path().join("no-space"), full)
            .await
            .is_err()
    );
    let owner = ownership::acquire_path(&source, false)?;
    assert!(
        take(&source, &home.path().join("busy"), options(home.path()))
            .await
            .is_err()
    );
    drop(owner);
    let good = take(
        &source,
        &home.path().join("after-refusals"),
        options(home.path()),
    )
    .await?;
    assert_eq!(good.evidence.inventory.rows, 7);
    assert_eq!(std::fs::read(&source)?, original);
    for entry in std::fs::read_dir(home.path())? {
        assert!(
            !entry?
                .file_name()
                .to_string_lossy()
                .starts_with(".bitrouter-offline-")
        );
    }
    Ok(())
}
