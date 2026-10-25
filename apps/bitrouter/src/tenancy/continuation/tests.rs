use super::*;
#[cfg(unix)]
mod quarantine;
#[cfg(unix)]
mod unwind;
use crate::tenancy::{Service, actor::OperationLimits, resources::Resources, store};
#[cfg(unix)]
use crate::tenancy::{
    actor::{Liveness, Worker},
    transaction::{Disposition, Phase, Proof},
};
use anyhow::Context;
use std::sync::Arc;
#[cfg(unix)]
use std::{
    sync::{RwLock, atomic::AtomicBool},
    time::Duration,
};

async fn database() -> Result<DatabaseConnection> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok(db)
}

#[tokio::test]
async fn pin_survives_empty_catalog_and_rejects_missing_changed_key_and_identity() -> Result<()> {
    let db = database().await?;
    let initial = store::load(&db, chrono::Utc::now()).await?;
    let installation = initial.installation.clone();
    let resources = Arc::new(Resources::with_key(installation.clone(), [51; 32])?);
    let service = Service::start_prepared(
        db.clone(),
        None,
        OperationLimits::default(),
        initial,
        resources,
    )
    .await?;
    service.shutdown().await?;
    ensure!(has_epoch(&db).await?);
    for key in [None, Some([52; 32])] {
        let initial = store::load(&db, chrono::Utc::now()).await?;
        let resources = match key {
            Some(key) => Resources::with_key(installation.clone(), key)?,
            None => Resources::open(&db, &installation, &Default::default()).await?,
        };
        ensure!(
            Service::start_prepared(
                db.clone(),
                None,
                OperationLimits::default(),
                initial,
                Arc::new(resources)
            )
            .await
            .is_err()
        );
    }
    let initial = store::load(&db, chrono::Utc::now()).await?;
    let service = Service::start_prepared(
        db.clone(),
        None,
        OperationLimits::default(),
        initial,
        Arc::new(Resources::with_key(installation, [51; 32])?),
    )
    .await?;
    service.shutdown().await?;
    db.execute(sea_orm::Statement::from_string(
        db.get_database_backend(),
        "DELETE FROM tenancy_installation".to_owned(),
    ))
    .await?;
    ensure!(
        store::load(&db, chrono::Utc::now()).await.is_err(),
        "a native epoch must prevent installation reassignment"
    );
    Ok(())
}

#[cfg(unix)]
#[tokio::test]
async fn abandoned_startup_waiter_keeps_pin_transaction_custody_until_commit() -> Result<()> {
    for phase in [Phase::Beginning, Phase::Committing, Phase::RollingBack] {
        let directory = tempfile::tempdir()?;
        let path = directory.path().join("native-bootstrap.sqlite");
        let owner = crate::tenancy::ownership::acquire_path(&path, true)?;
        let db =
            crate::db::connect_tenancy(&format!("sqlite://{}", path.display()), &owner).await?;
        crate::db::run_migrations(&db).await?;
        if phase == Phase::RollingBack {
            db.execute_unprepared("CREATE TRIGGER native_epoch_dml_fault BEFORE INSERT ON tenancy_continuation_epoch BEGIN SELECT RAISE(ABORT, 'epoch fault'); END").await?;
        }
        let initial = store::load(&db, chrono::Utc::now()).await?;
        let resources = Arc::new(Resources::with_key(initial.installation.clone(), [53; 32])?);
        let live = Arc::new(AtomicBool::new(true));
        let proof = Proof::new(live.clone());
        let gate = proof.hold(phase);
        let worker = Worker {
            db: db.clone(),
            snapshot: Arc::new(RwLock::new(Arc::new(initial))),
            live: Liveness(live.clone()),
            owner: Some(owner),
            limits: OperationLimits::default(),
            resources,
            proof: proof.clone(),
            admission: crate::tenancy::admission::Admission::new(),
            http_continuation: crate::continuation::tenant::state::State::new(
                &crate::config::ContinuationConfig::default(),
                uuid::Uuid::new_v4().to_string(),
            )?,
            // Integration-specific: the parent Eval stage added these fields
            // after the overlay's captured b3888397 source baseline.
            #[cfg(feature = "core")]
            eval: crate::tenancy::eval::State::new(live),
            #[cfg(feature = "core")]
            accepting: Arc::new(std::sync::Mutex::new(true)),
        };
        let (commands, receiver) = tokio::sync::mpsc::channel(16);
        let (safety, safety_receiver) = tokio::sync::mpsc::channel(8);
        let (_completions, completion_receiver) = tokio::sync::mpsc::channel(128);
        let (initialized, waiter) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(worker.run(
            receiver,
            safety_receiver,
            completion_receiver,
            tokio_util::sync::CancellationToken::new(),
            None,
            initialized,
        ));
        tokio::time::timeout(Duration::from_secs(5), gate.entered.notified()).await?;
        drop(waiter);
        tokio::task::yield_now().await;
        ensure!(!task.is_finished());
        ensure!(crate::tenancy::ownership::acquire_path(&path, false).is_err());
        ensure!(proof.disposition() == Disposition::Pending);
        proof.release_gate();
        tokio::time::timeout(Duration::from_secs(5), task).await??;
        proof.ensure_clean()?;
        let committed = phase != Phase::RollingBack;
        ensure!(
            proof.disposition()
                == if committed {
                    Disposition::Committed
                } else {
                    Disposition::RolledBack
                }
        );
        ensure!(has_epoch(&db).await? == committed);
        ensure!(
            commands.is_closed() && safety.is_closed(),
            "abandoned startup left a serving receiver"
        );
        db.close().await?;
        tokio::time::timeout(Duration::from_secs(5), async {
            loop {
                match crate::tenancy::ownership::acquire_path(&path, false) {
                    Ok(owner) => {
                        drop(owner);
                        return Ok::<_, anyhow::Error>(());
                    }
                    Err(error)
                        if matches!(
                            error.downcast_ref::<std::fs::TryLockError>(),
                            Some(std::fs::TryLockError::WouldBlock)
                        ) =>
                    {
                        tokio::time::sleep(Duration::from_millis(10)).await
                    }
                    Err(error) => return Err(error),
                }
            }
        })
        .await??;
    }
    Ok(())
}

#[tokio::test]
async fn wrong_key_import_cannot_poison_an_empty_pinned_catalog() -> Result<()> {
    let db = database().await?;
    let initial = store::load(&db, chrono::Utc::now()).await?;
    let installation = initial.installation.clone();
    let resources = Arc::new(Resources::with_key(installation.clone(), [61; 32])?);
    let service = Service::start_prepared(
        db.clone(),
        None,
        OperationLimits::default(),
        initial,
        resources.clone(),
    )
    .await?;
    service.shutdown().await?;
    let epoch = |db: DatabaseConnection| async move {
        let row = db
            .query_one(sea_orm::Statement::from_string(
                db.get_database_backend(),
                "SELECT installation_id, key_id, created_at FROM tenancy_continuation_epoch"
                    .to_owned(),
            ))
            .await?
            .context("epoch")?;
        Ok::<_, anyhow::Error>((
            row.try_get::<String>("", "installation_id")?,
            row.try_get::<String>("", "key_id")?,
            row.try_get::<String>("", "created_at")?,
        ))
    };
    let before = epoch(db.clone()).await?;
    let import = serde_json::to_vec(
        &serde_json::json!({"format":1,"installation_id":installation,"resources":[{
            "reference":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential","resource_id":"slot","revision_id":"r1"},
            "definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"must-not-be-written"}}}
        }]}),
    )?;
    let initial = store::load(&db, chrono::Utc::now()).await?;
    ensure!(
        Service::start_resources(
            db.clone(),
            None,
            OperationLimits::default(),
            initial,
            Arc::new(Resources::with_key(installation, [62; 32])?),
            Some(import),
            None
        )
        .await
        .is_err()
    );
    ensure!(epoch(db.clone()).await? == before);
    let rows = db
        .query_all(sea_orm::Statement::from_string(
            db.get_database_backend(),
            "SELECT 1 FROM tenancy_resource_revisions LIMIT 1".to_owned(),
        ))
        .await?;
    ensure!(
        rows.is_empty(),
        "failed K2 startup imported ciphertext before checking K1 pin"
    );
    let initial = store::load(&db, chrono::Utc::now()).await?;
    let restored =
        Service::start_prepared(db, None, OperationLimits::default(), initial, resources).await?;
    restored.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn oversized_epoch_projection_fails_closed() -> Result<()> {
    let db = database().await?;
    let initial = store::load(&db, chrono::Utc::now()).await?;
    let resources = Arc::new(Resources::with_key(initial.installation.clone(), [54; 32])?);
    let service = Service::start_prepared(
        db.clone(),
        None,
        OperationLimits::default(),
        initial,
        resources.clone(),
    )
    .await?;
    service.shutdown().await?;
    db.execute(sea_orm::Statement::from_sql_and_values(
        db.get_database_backend(),
        "UPDATE tenancy_continuation_epoch SET key_id = ?",
        ["x".repeat(128 * 1024).into()],
    ))
    .await?;
    let initial = store::load(&db, chrono::Utc::now()).await?;
    ensure!(
        Service::start_prepared(db, None, OperationLimits::default(), initial, resources)
            .await
            .is_err()
    );
    Ok(())
}

#[test]
fn domain_owner_and_budget_are_enforced_without_exposing_the_key() -> Result<()> {
    let crypto = Crypto::new(
        "installation".try_into()?,
        DeploymentKey::from_bytes([55; 32])?,
    );
    let owner = ("tenant", "principal");
    let bytes = crypto.seal("continuation.native.v1", &owner, &"opaque", 4096)?;
    let clear: String = crypto.open("continuation.native.v1", &owner, &bytes, 4096)?;
    ensure!(clear == "opaque");
    ensure!(
        crypto
            .open::<String>("continuation.origin.v1", &owner, &bytes, 4096)
            .is_err()
    );
    ensure!(
        crypto
            .open::<String>(
                "continuation.native.v1",
                &("tenant-other", "principal"),
                &bytes,
                4096
            )
            .is_err()
    );
    ensure!(
        crypto
            .seal("configuration", &owner, &"opaque", 4096)
            .is_err()
    );
    ensure!(bounded_json(&"x".repeat(4096), 64).is_err());
    ensure!(
        crypto
            .seal("continuation.native.v1", &owner, &"x".repeat(4096), 64)
            .is_err()
    );
    ensure!(
        crypto
            .open::<String>("continuation.native.v1", &owner, &bytes, 16)
            .is_err()
    );
    let decoded: String = crypto
        .open("continuation.native.v1", &owner, &bytes, 4096)
        .context("failed operations must leave key usable")?;
    ensure!(decoded == "opaque");
    Ok(())
}
