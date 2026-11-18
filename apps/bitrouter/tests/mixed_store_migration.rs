//! Offline ownership split using a real M0 schema and captured v7 BRO records.
#![cfg(any(target_os = "linux", target_os = "macos"))]

use anyhow::{Context, Result, ensure};
use bitrouter::db::{
    self,
    mixed_store::{self, Owner},
};
use bitrouter::storage;
use sea_orm::{
    ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement,
    sqlx::sqlite::{SqliteConnectOptions, SqlitePoolOptions},
};
use std::{
    fs,
    os::unix::fs::PermissionsExt,
    path::{Path, PathBuf},
};

const CAPTURE: &[u8] = include_bytes!("fixtures/baseline-runtime-v7/database.sqlite.zst");
const THREAD: &str = "1149fbda-eb06-43b7-8d0e-108e857d9c2d";

async fn connection(path: &Path, readonly: bool) -> Result<DatabaseConnection> {
    let options = SqliteConnectOptions::new()
        .filename(path)
        .read_only(readonly);
    Ok(DatabaseConnection::from(
        SqlitePoolOptions::new()
            .max_connections(1)
            .connect_with(options)
            .await?,
    ))
}

async fn fresh(dir: &Path) -> Result<(PathBuf, DatabaseConnection)> {
    let path = dir.join("source.sqlite");
    fs::File::create_new(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let db = connection(&path, false).await?;
    // A product's current initializer evolves independently of this source format.
    #[derive(serde::Deserialize)]
    struct Object {
        r#type: String,
        sql: String,
    }
    let catalog: Vec<Object> = serde_json::from_str(include_str!(
        "../src/db/migration/baseline/fixtures/sqlite-current-schema.json"
    ))?;
    for kind in ["table", "index"] {
        for obj in catalog.iter().filter(|o| o.r#type == kind) {
            db.execute_unprepared(&obj.sql).await?;
        }
    }
    db.execute_unprepared("INSERT INTO seaql_migrations VALUES('m20261118_000001_model_policy_store',1); INSERT INTO bro_runtime_ownership(id,generation) VALUES(1,0)").await?;
    Ok((path, db))
}

async fn fixture(dir: &Path) -> Result<PathBuf> {
    let (path, db) = fresh(dir).await?;
    let capture = dir.join("v7.sqlite");
    fs::write(&capture, zstd::stream::decode_all(CAPTURE)?)?;
    db.execute(Statement::from_sql_and_values(
        DatabaseBackend::Sqlite,
        "ATTACH DATABASE ? AS captured",
        [capture.to_str().context("capture path")?.into()],
    ))
    .await?;
    for table in [
        "bro_executions",
        "bro_execution_index",
        "bro_execution_records",
        "bro_acceptance_keys",
        "bro_runtime_owners",
    ] {
        db.execute_unprepared(&format!(
            "INSERT INTO main.{table} SELECT * FROM captured.{table}"
        ))
        .await?;
    }
    db.execute_unprepared("UPDATE main.bro_runtime_ownership SET (generation,server_instance_id,stopped_at_ms)=(SELECT generation,server_instance_id,stopped_at_ms FROM captured.bro_runtime_ownership WHERE id=1) WHERE id=1").await?;
    db.execute_unprepared("DETACH DATABASE captured").await?;
    // Synthetic service rows exercise opaque data, bindings and pending
    // obligations. The BRO checkpoint and tool record bytes above are real.
    db.execute_unprepared(r#"
        INSERT INTO tenancy_installation VALUES(1,'installation',5,9,0,0,0,'now');
        INSERT INTO tenancy_tenants VALUES('tenant','r1','active','now');
        INSERT INTO tenancy_principals VALUES('principal','r1','issuer','subject','now');
        INSERT INTO tenancy_resource_revisions VALUES('tenant','tenant','policy','model','p1','tenancy-model-policy-v1','{"ciphertext":"opaque-policy"}','[]','policy-digest','now');
        INSERT INTO tenancy_resource_lifecycle VALUES('tenant','tenant','policy','model','p1','installation',1,0,9);
        INSERT INTO tenancy_configurations VALUES('tenant','config1',1,'{"policy":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"policy","resource_id":"model","revision_id":"p1"},"routes":[],"learning":{"mode":"off","max_evaluations":0},"generation_defaults":{"temperature":0,"max_output_tokens":8},"limits":{"active_executions":1,"queued_work":0,"learning_jobs":0}}','compiler','digest','{}','[]','principal',9,'now');
        INSERT INTO tenancy_active_configurations VALUES('tenant','config1',9);
        INSERT INTO tenancy_execution_receipts VALUES('installation','tenant','exec','principal','corr','{"outcome":"unknown"}','receipt-digest','{}',21,1,'now',0,1);
        INSERT INTO tenancy_unknown_obligations VALUES('installation','tenant','exec',1,100,0,130,0,0);
        INSERT INTO tenancy_usage_evidence VALUES('installation','tenant','exec',1,'receipt-digest','usage-digest','{"tokens":17}',13,0);
        INSERT INTO tenancy_dispatch_intents VALUES('installation','tenant','pending','{"pending":true}','digest',16);
        INSERT INTO acp_canonical_sessions(session_key,owner,source,native_session_id,head,history_origin,deleted) VALUES('session','local','native','native-id',1,'capture',0);
        INSERT INTO acp_checkpoints(checkpoint_id,session_key,manifest_json,watermark,deleted) VALUES('checkpoint','session','{}',1,0);
        INSERT INTO acp_checkpoint_members(checkpoint_id,session_key) VALUES('checkpoint','session');
        INSERT INTO bro_managed_sessions(id,revision,payload_bytes,payload) VALUES('managed',1,2,'{}');
        INSERT INTO checkpoint_evolution(record_id,scope_id,kind,revision,body) VALUES
            ('gw','gw','gateway_request',1,'{}'),('budget','bro','owned_judge_budget',9,'{"spent":1234}');
        UPDATE sqlite_sequence SET seq=200 WHERE name='bro_execution_index';
        INSERT INTO sqlite_sequence(name,seq) VALUES('eval_admission_events',89);
    "#).await?;
    db.close().await?;
    Ok(path)
}

async fn scalar(db: &DatabaseConnection, sql: &str) -> Result<i64> {
    Ok(db
        .query_one(Statement::from_string(DatabaseBackend::Sqlite, sql))
        .await?
        .context("expected scalar")?
        .try_get_by_index(0)?)
}

#[tokio::test]
async fn publishes_both_owners_and_preserves_recovery_facts_on_reopen() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = fixture(dir.path()).await?;
    let original = fs::read(&source)?;
    let destination = dir.path().join("staged");
    let result = mixed_store::stage(&source, &destination).await?;
    ensure!(!result.dispatch_allowed);
    ensure!(fs::read(&source)? == original && fs::read(&result.rollback)? == original);
    ensure!(
        result.bro_rows["bro_execution_records"] == 284
            && result.bitrouter_rows["tenancy_unknown_obligations"] == 1
    );
    ensure!(
        mixed_store::verify_owner(&result.bro, Owner::BitRouter)
            .await
            .is_err()
    );
    ensure!(
        mixed_store::verify_owner(&result.bitrouter, Owner::Bro)
            .await
            .is_err()
    );
    ensure!(
        mixed_store::verify_owner(&source, Owner::Bro)
            .await
            .is_err()
    );
    for _ in 0..2 {
        let service = connection(&result.bitrouter, true).await?;
        ensure!(scalar(&service,"SELECT count(*) FROM sqlite_schema WHERE name IN ('bro_executions','acp_checkpoints','bro_managed_sessions')").await?==0);
        ensure!(scalar(&service,"SELECT count(*) FROM tenancy_active_configurations a JOIN tenancy_configurations c USING(tenant_id,revision_id) JOIN tenancy_resource_revisions r ON r.revision_id=json_extract(c.document_json,'$.policy.revision_id') WHERE r.compiler_version='tenancy-model-policy-v1'").await?==1);
        ensure!(scalar(&service,"SELECT json_extract(u.evidence_json,'$.tokens') FROM tenancy_unknown_obligations o JOIN tenancy_execution_receipts r USING(installation_id,tenant_id,execution_id) JOIN tenancy_usage_evidence u USING(installation_id,tenant_id,execution_id) WHERE o.source_mask=1 AND r.unknown_outcome=1").await?==17);
        ensure!(
            scalar(
                &service,
                "SELECT count(*) FROM tenancy_dispatch_intents WHERE execution_id='pending'"
            )
            .await?
                == 1
        );
        ensure!(
            scalar(
                &service,
                "SELECT seq FROM sqlite_sequence WHERE name='eval_admission_events'"
            )
            .await?
                == 89
        );
        ensure!(
            scalar(
                &service,
                "SELECT count(*) FROM checkpoint_evolution WHERE kind='gateway_request'"
            )
            .await?
                == 1
        );
        service.close().await?;
        let bro = connection(&result.bro, true).await?;
        ensure!(scalar(&bro,"SELECT count(*) FROM sqlite_schema WHERE name IN ('tenancy_credentials','tenancy_execution_receipts','requests','api_keys')").await?==0);
        ensure!(scalar(&bro,"SELECT count(*) FROM acp_checkpoints JOIN acp_checkpoint_members USING(checkpoint_id,session_key)").await?==1);
        ensure!(scalar(&bro,"SELECT json_extract(body,'$.spent') FROM checkpoint_evolution WHERE kind='owned_judge_budget'").await?==1234);
        ensure!(
            scalar(
                &bro,
                "SELECT seq FROM sqlite_sequence WHERE name='bro_execution_index'"
            )
            .await?
                == 200
        );
        ensure!(
            scalar(
                &bro,
                &format!("SELECT version FROM bro_executions WHERE id='{THREAD}'")
            )
            .await?
                == 284
        );
        let confirmed = scalar(&bro, r#"SELECT count(*) FROM bro_execution_records r,
            json_each(CASE WHEN json_valid(r.payload) THEN r.payload ELSE '{}' END,'$.event.changes') c
            WHERE json_extract(c.value,'$.kind')='tool_result' AND json_extract(c.value,'$.effect')='completed'
              AND json_extract(c.value,'$.message.content[0].call_id')='read-evidence'"#).await?;
        ensure!(
            confirmed == 1,
            "confirmed M0 read must survive exactly once"
        );
        #[cfg(feature = "harness")]
        {
            use bitrouter::{
                agent_store::DatabaseExecutionStore, harness::storage::ExecutionStore,
            };
            let history = DatabaseExecutionStore::new(bro.clone())
                .load(THREAD)
                .await
                .map_err(anyhow::Error::msg)?
                .context("missing retained journal")?;
            let expected = zstd::stream::decode_all(
                &include_bytes!("fixtures/baseline-runtime-v7/journal.json.zst")[..],
            )?;
            ensure!(
                serde_json::to_value(&history.records)?
                    == serde_json::from_slice::<serde_json::Value>(&expected)?
            );
        }
        bro.close().await?;
    }
    ensure!(mixed_store::stage(&source, &destination).await.is_err());
    ensure!(fs::read(&source)? == original);
    Ok(())
}

#[tokio::test]
async fn refuses_busy_drifted_historical_and_unclassified_sources() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let path = dir.path().join("busy.sqlite");
    fs::File::create_new(&path)?;
    fs::set_permissions(&path, fs::Permissions::from_mode(0o600))?;
    let dest = dir.path().join("result");
    let writer = db::connect(&format!("sqlite://{}", path.display())).await?;
    ensure!(mixed_store::stage(&path, &dest).await.is_err());
    writer.close().await?;
    for statement in [
        "CREATE TABLE new_owner_data(id INTEGER)",
        "UPDATE seaql_migrations SET version='m20240101_legacy'",
        "INSERT INTO checkpoint_evolution(record_id,scope_id,kind,revision,body) VALUES('ambiguous','scope','future_kind',0,'{}')",
        "INSERT INTO checkpoint_evolution(record_id,scope_id,kind,revision,body) VALUES('pending','scope','unreviewed_paid_work',0,'{}')",
        "INSERT INTO acp_checkpoints(checkpoint_id,session_key,manifest_json,watermark,deleted) VALUES('orphan','missing','{}',0,0)",
    ] {
        let case = tempfile::tempdir_in(dir.path())?;
        let (path, db) = fresh(case.path()).await?;
        db.execute_unprepared(statement).await?;
        db.close().await?;
        let original = fs::read(&path)?;
        ensure!(mixed_store::stage(&path, &dest).await.is_err());
        ensure!(!dest.exists() && fs::read(&path)? == original);
    }
    Ok(())
}

#[tokio::test]
async fn failure_after_service_import_can_retry_without_reusing_partial_work() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = fixture(dir.path()).await?;
    let db = connection(&source, false).await?;
    db.execute_unprepared("UPDATE sqlite_sequence SET seq=0 WHERE name='bro_execution_index'")
        .await?;
    db.close().await?;
    let dest = dir.path().join("both");
    let error = mixed_store::stage(&source, &dest)
        .await
        .err()
        .context("invalid sequence accepted")?;
    ensure!(format!("{error:#}").contains("high-water"), "{error:#}");
    ensure!(!dest.exists());
    let mut staged = fs::read_dir(dir.path())?
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .filter(|f| {
            f.file_name()
                .to_string_lossy()
                .starts_with(".bitrouter-store-split-")
        });
    let abandoned = staged.next().context("missing abandoned attempt")?.path();
    let partial = abandoned.join("bitrouter/bitrouter.db");
    ensure!(
        mixed_store::verify_owner(&partial, Owner::BitRouter)
            .await
            .is_err()
    );
    let staged_service = connection(&partial, true).await?;
    ensure!(
        scalar(
            &staged_service,
            "SELECT count(*) FROM tenancy_unknown_obligations"
        )
        .await?
            == 1
    );
    staged_service.close().await?;
    let db = connection(&source, false).await?;
    db.execute_unprepared("UPDATE sqlite_sequence SET seq=201 WHERE name='bro_execution_index'")
        .await?;
    db.close().await?;
    let result = mixed_store::stage(&source, &dest).await?;
    let db = connection(&result.bro, true).await?;
    ensure!(
        scalar(
            &db,
            "SELECT seq FROM sqlite_sequence WHERE name='bro_execution_index'"
        )
        .await?
            == 201
    );
    db.close().await?;
    ensure!(abandoned.exists());
    Ok(())
}

#[tokio::test]
async fn explicitly_activates_a_clean_pair_and_rejects_staged_gateway_startup() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (path, db) = fresh(dir.path()).await?;
    db.execute_unprepared("INSERT INTO users VALUES('existing-user','now')")
        .await?;
    db.close().await?;
    let bytes = fs::read(&path)?;
    let staged = mixed_store::stage(&path, &dir.path().join("inspect")).await?;
    ensure!(!staged.dispatch_allowed);
    ensure!(!dir.path().join("inspect/bro/ACTIVATED").exists());
    ensure!(
        db::connect(&format!("sqlite://{}", staged.bitrouter.display()))
            .await
            .is_err()
    );

    let active = dir.path().join("active");
    let report = mixed_store::activate(&path, &active).await?;
    ensure!(report.dispatch_allowed && report.blockers.is_empty());
    let pair = report.stores.context("missing active pair")?;
    ensure!(
        pair.dispatch_allowed && fs::read(&path)? != bytes && fs::read(&pair.rollback)? == bytes
    );
    ensure!(fs::read(active.join("bro/ACTIVATED"))? == b"m20261118_000004_bro_sqlite\n");
    for _ in 0..2 {
        let gateway = db::connect(&format!("sqlite://{}", pair.bitrouter.display())).await?;
        db::run_migrations(&gateway).await?;
        ensure!(
            scalar(
                &gateway,
                "SELECT count(*) FROM users WHERE id='existing-user'"
            )
            .await?
                == 1
        );
        gateway.close().await?;
        mixed_store::verify_owner(&pair.bro, Owner::Bro).await?;
    }
    ensure!(pair.bitrouter == active.canonicalize()?.join("bitrouter/bitrouter.db"));
    ensure!(mixed_store::activate(&path, &active).await.is_err());
    let second = dir.path().join("second-owner");
    ensure!(mixed_store::activate(&path, &second).await.is_err() && !second.exists());
    ensure!(
        db::connect(&format!("sqlite://{}", path.display()))
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn unknown_and_retained_work_leave_activation_destination_absent() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let source = fixture(dir.path()).await?;
    let before = fs::read(&source)?;
    let destination = dir.path().join("active");
    let result = mixed_store::activate(&source, &destination).await?;
    ensure!(!result.dispatch_allowed && result.stores.is_none() && !destination.exists());
    for expected in [
        "execution obligations",
        "dispatch intent",
        "bro_execution_records",
        "bro_managed_sessions",
    ] {
        ensure!(result.blockers.iter().any(|b| b.contains(expected)));
    }
    ensure!(fs::read(&source)? == before);
    ensure!(
        fs::read_dir(dir.path())?
            .collect::<std::io::Result<Vec<_>>>()?
            .iter()
            .all(|e| !e
                .file_name()
                .to_string_lossy()
                .starts_with(".bitrouter-store-split-"))
    );
    Ok(())
}

#[tokio::test]
async fn fresh_gateway_matches_projection_and_rejects_drift_before_wal() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (source, db) = fresh(dir.path()).await?;
    db.close().await?;
    let active = mixed_store::activate(&source, &dir.path().join("active"))
        .await?
        .stores
        .context("missing active stores")?;
    let fresh = dir.path().join("new.sqlite");
    let gateway = db::connect(&format!("sqlite://{}", fresh.display())).await?;
    db::run_migrations(&gateway).await?;
    gateway.close().await?;
    mixed_store::verify_owner(&fresh, Owner::BitRouter).await?;
    let checked = connection(&fresh, true).await?;
    checked
        .execute(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "ATTACH ? AS migrated",
            [active.bitrouter.to_str().context("path")?.into()],
        ))
        .await?;
    for (left, right) in [("main", "migrated"), ("migrated", "main")] {
        ensure!(scalar(&checked,&format!("SELECT count(*) FROM (SELECT type,name,tbl_name,sql FROM {left}.sqlite_schema EXCEPT SELECT type,name,tbl_name,sql FROM {right}.sqlite_schema)")).await?==0);
    }
    checked.close().await?;
    let drift = connection(&active.bitrouter, false).await?;
    drift
        .execute_unprepared("CREATE TRIGGER unexpected AFTER INSERT ON users BEGIN SELECT 1; END")
        .await?;
    drift.close().await?;
    let before = fs::read(&active.bitrouter)?;
    ensure!(
        db::connect(&format!("sqlite://{}", active.bitrouter.display()))
            .await
            .is_err()
    );
    ensure!(fs::read(&active.bitrouter)? == before);
    Ok(())
}

#[path = "mixed_store_migration/sealed.rs"]
mod sealed;

#[tokio::test]
async fn malformed_virtual_key_expiry_stops_activation_without_a_tenant_manifest() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let (source, db) = fresh(dir.path()).await?;
    db.execute_unprepared("INSERT INTO users VALUES ('operator','2026-01-01T00:00:00Z'); INSERT INTO api_keys(id,user_id,key_hash,active,expires_at,created_at) VALUES ('fixture','operator','fixture-hash',1,'invalid-date','2026-01-01T00:00:00Z')").await?;
    ensure!(
        bitrouter::auth::db::find_key_by_hash(&db, "fixture-hash")
            .await
            .is_err()
    );
    db.close().await?;
    let before = fs::read(&source)?;
    let dest = dir.path().join("denied");
    ensure!(mixed_store::activate(&source, &dest).await.is_err());
    ensure!(!dest.exists() && fs::read(&source)? == before);
    Ok(())
}
