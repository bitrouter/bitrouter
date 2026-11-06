use super::*;
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use std::{os::unix::fs::PermissionsExt, sync::Arc, time::Duration};

#[derive(Default)]
pub(super) struct Gate {
    pub(super) entered: CancellationToken,
    pub(super) release: CancellationToken,
}

#[tokio::test]
async fn large_projection_keeps_inventory_and_rejects_input_scratch_alias() -> Result<()> {
    let home = tempfile::tempdir()?;
    std::fs::set_permissions(home.path(), std::fs::Permissions::from_mode(0o700))?;
    let (bundle, digest) = bundle(home.path()).await?;
    let alias = home.path().join("bundle-alias");
    std::os::unix::fs::symlink(&bundle, &alias)?;
    let options = super::super::large::Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: alias,
        min_free_bytes: 1,
    };
    let projection = home.path().join("refused");
    ensure!(
        open_inner(
            &bundle,
            &projection,
            &digest,
            CancellationToken::new(),
            super::super::ValidationBudget::default(),
            Some(options),
            None
        )
        .await
        .is_err()
    );
    ensure!(std::fs::read_dir(&bundle)?.count() == 4 && !projection.exists());
    super::super::read_bundle(&bundle, Some(&digest), "verify").await?;
    let options = super::super::large::Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.path().canonicalize()?,
        min_free_bytes: 1,
    };
    let projection = home.path().join("large-projection");
    let archive = open_inner(
        &bundle,
        &projection,
        &digest,
        CancellationToken::new(),
        super::super::ValidationBudget::default(),
        Some(options),
        None,
    )
    .await?;
    ensure!(archive.installation().as_str() == "audit-budget");
    archive.shutdown().await?;
    ensure!(
        std::fs::read(bundle.join("snapshot.sqlite"))?
            == std::fs::read(projection.join("audit.sqlite"))?
    );
    Ok(())
}

async fn bundle(home: &Path) -> Result<(PathBuf, String)> {
    let source = home.join("source.sqlite");
    let owner = crate::tenancy::ownership::acquire_path(&source, true)?;
    let db =
        crate::db::connect_tenancy(&format!("sqlite://{}", owner.path.display()), &owner).await?;
    crate::db::run_migrations(&db).await?;
    db.execute(Statement::from_string(DatabaseBackend::Sqlite,
        "INSERT INTO tenancy_installation(singleton,schema_version,installation_id,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,2,'audit-budget',0,0,0,0,'2026-01-01T00:00:00Z')"
    )).await?;
    db.close().await?;
    drop(owner);
    std::fs::set_permissions(&source, std::fs::Permissions::from_mode(0o600))?;
    let bundle = home.join("bundle");
    super::super::snapshot(&source, &bundle).await?;
    let evidence = std::fs::read(bundle.join("evidence.json"))?;
    Ok((bundle, files::sha(&evidence)))
}

/// The real projection owner survives the former observer timeout and finishes.
#[tokio::test(flavor = "current_thread")]
async fn complete_validation_has_no_history_duration_timeout() -> Result<()> {
    let home = tempfile::tempdir()?;
    let (bundle, digest) = bundle(home.path()).await?;
    let projection = home.path().join("projection");
    let gate = Arc::new(Gate::default());
    let mut opening = Box::pin(open_with_budget(
        &bundle,
        &projection,
        &digest,
        CancellationToken::new(),
        super::super::ValidationBudget::default(),
        Some(gate.clone()),
    ));
    ensure!(futures::poll!(&mut opening).is_pending());
    tokio::time::timeout(Duration::from_secs(10), gate.entered.cancelled()).await?;
    let database = projection.join("audit.sqlite");
    let held = crate::tenancy::ownership::acquire_path(&database, false).is_err();
    tokio::time::pause();
    tokio::time::advance(Duration::from_secs(121)).await;
    let pending = futures::poll!(&mut opening).is_pending();
    tokio::time::resume();
    gate.release.cancel();
    ensure!(
        pending,
        "complete validation ended at the old observer deadline"
    );
    let opened = tokio::time::timeout(Duration::from_secs(10), &mut opening).await;
    if let Ok(Ok(reader)) = opened {
        reader.shutdown().await?;
    } else {
        anyhow::bail!("complete audit validation did not finish after the old deadline");
    }
    ensure!(
        held && pending,
        "observer time released a live validation owner"
    );
    drop(crate::tenancy::ownership::acquire_path(&database, false)?);
    Ok(())
}

/// Dropping a reply cancels work but cannot discard the actual SQLite owner.
#[tokio::test(flavor = "current_thread")]
async fn abandoned_validation_keeps_projection_until_worker_close() -> Result<()> {
    let home = tempfile::tempdir()?;
    let (bundle, digest) = bundle(home.path()).await?;
    let projection = home.path().join("projection");
    let gate = Arc::new(Gate::default());
    let mut opening = Box::pin(open_with_budget(
        &bundle,
        &projection,
        &digest,
        CancellationToken::new(),
        super::super::ValidationBudget::default(),
        Some(gate.clone()),
    ));
    ensure!(futures::poll!(&mut opening).is_pending());
    tokio::time::timeout(Duration::from_secs(10), gate.entered.cancelled()).await?;
    let database = projection.join("audit.sqlite");
    drop(opening);
    tokio::task::yield_now().await;
    let held = crate::tenancy::ownership::acquire_path(&database, false).is_err();
    gate.release.cancel();
    let owner = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            if let Ok(owner) = crate::tenancy::ownership::acquire_path(&database, false) {
                break owner;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await?;
    ensure!(
        held,
        "abandoned observer released the owner before worker close"
    );
    drop(owner);
    Ok(())
}
