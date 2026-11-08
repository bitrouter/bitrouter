//! Run once per supported server backend, only inside a confirmed owned database.
use super::*;
use futures::FutureExt;
use sea_orm::{ConnectOptions, Database};
use std::time::Duration;

async fn connect(url: &str) -> Result<DatabaseConnection> {
    let mut options = ConnectOptions::new(url);
    options
        .min_connections(1)
        .max_connections(1)
        .connect_timeout(Duration::from_secs(5))
        .sqlx_logging(false);
    Database::connect(options)
        .await
        .map_err(|_| anyhow::anyhow!("shared health fixture connection failed"))
}

async fn marker(db: &DatabaseConnection) -> Result<i32> {
    Ok(db
        .query_one(Statement::from_string(
            db.get_database_backend(),
            "SELECT schema_version FROM tenancy_installation".to_owned(),
        ))
        .await?
        .context("marker")?
        .try_get("", "schema_version")?)
}

async fn campaign(db: &DatabaseConnection) -> Result<()> {
    let backend = db.get_database_backend();
    let before = Migrator::migrations()
        .iter()
        .position(|migration| migration.name() == MIGRATION)
        .context("migration registration")?;
    Migrator::up(db, Some(u32::try_from(before)?)).await?;
    let fields: Vec<(&str, sea_orm::Value)> = vec![
        ("singleton", 1_i32.into()),
        ("installation_id", "installation".into()),
        ("schema_version", 3_i32.into()),
        ("authority_revision", 0_i64.into()),
        ("operation_ids", 0_i64.into()),
        ("safety_operation_ids", 0_i64.into()),
        ("saturated", false.into()),
        ("created_at", "2026-10-25T00:00:00Z".into()),
    ];
    let mut insert = Query::insert();
    insert
        .into_table(Alias::new("tenancy_installation"))
        .columns(fields.iter().map(|(name, _)| Alias::new(*name)))
        .values(fields.into_iter().map(|(_, v)| v.into()))?;
    db.execute(backend.build(&insert)).await?;
    let original = candidate("installation", "tenant", "historical", "principal")?
        .with_usage_evidence(crate::tenancy::receipts::usage_evidence::Snapshot::default())?;
    ensure!(commit(db, &original).await?);
    let ledger = Migrator::get_migration_models(db).await?;
    // Both marker and framework ledger must roll back on the same real failure.
    if backend == DatabaseBackend::Postgres {
        db.execute_unprepared(&format!("CREATE FUNCTION reject_shared_fence() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.version='{MIGRATION}' THEN RAISE EXCEPTION 'injected shared ledger failure'; END IF; RETURN NEW; END $$")).await?;
        db.execute_unprepared("CREATE TRIGGER reject_shared_fence BEFORE INSERT ON seaql_migrations FOR EACH ROW EXECUTE FUNCTION reject_shared_fence()").await?;
    } else {
        db.execute_unprepared(&format!("CREATE TRIGGER reject_shared_fence BEFORE INSERT ON seaql_migrations FOR EACH ROW BEGIN IF NEW.version='{MIGRATION}' THEN SIGNAL SQLSTATE '45000' SET MESSAGE_TEXT='injected shared ledger failure'; END IF; END")).await?;
    }
    ensure!(crate::db::run_migrations(db).await.is_err());
    ensure!(marker(db).await? == 3 && Migrator::get_migration_models(db).await? == ledger);
    if backend == DatabaseBackend::Postgres {
        db.execute_unprepared("DROP TRIGGER reject_shared_fence ON seaql_migrations")
            .await?;
        db.execute_unprepared("DROP FUNCTION reject_shared_fence()")
            .await?;
    } else {
        db.execute_unprepared("DROP TRIGGER reject_shared_fence")
            .await?;
    }
    for invalid in [1_i32, 2, 5] {
        let update = Query::update()
            .table(Alias::new("tenancy_installation"))
            .value(Alias::new("schema_version"), invalid)
            .to_owned();
        db.execute(backend.build(&update)).await?;
        ensure!(crate::db::run_migrations(db).await.is_err());
        ensure!(
            marker(db).await? == invalid && Migrator::get_migration_models(db).await? == ledger
        );
    }
    let update = Query::update()
        .table(Alias::new("tenancy_installation"))
        .value(Alias::new("schema_version"), 3)
        .to_owned();
    db.execute(backend.build(&update)).await?;
    crate::db::run_migrations(db).await?;
    ensure!(marker(db).await? == 4);
    management_mutation(db).await?;
    ensure!(marker(db).await? == 4);
    let current = lookup_candidate(
        db,
        &original.receipt.installation_id,
        &original.receipt.tenant_id,
        &original.receipt.execution_id,
    )
    .await?
    .context("historical receipt")?;
    ensure!(
        current.json == original.json
            && current.private_json == original.private_json
            && current.digest == original.digest
    );
    let original_evidence = original
        .usage_evidence
        .as_ref()
        .context("original evidence")?;
    let current_evidence = current
        .usage_evidence
        .as_ref()
        .context("current evidence")?;
    ensure!(
        current_evidence.json == original_evidence.json
            && current_evidence.digest == original_evidence.digest
    );
    let installed = Migrator::get_migration_models(db).await?;
    ensure!(installed.len() == Migrator::migrations().len());
    crate::db::run_migrations(db).await?;
    ensure!(Migrator::get_migration_models(db).await? == installed);
    Ok(())
}

#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with isolated PostgreSQL or MySQL CREATE/DROP DATABASE privileges; run on both backends"]
async fn shared_health_server_fence_ledger_atomicity_history_and_ordinary_writer() -> Result<()> {
    let source =
        std::env::var("BITROUTER_TEST_DATABASE_URL").context("dedicated database URL required")?;
    let mut target = url::Url::parse(&source)?;
    let backend = match target.scheme() {
        "postgres" | "postgresql" => {
            ensure!(cfg!(feature = "db-postgres"));
            DatabaseBackend::Postgres
        }
        "mysql" => {
            ensure!(cfg!(feature = "db-mysql"));
            DatabaseBackend::MySql
        }
        _ => anyhow::bail!("PostgreSQL or MySQL fixture required"),
    };
    ensure!(
        !target.path().trim_matches('/').is_empty(),
        "explicit admin database required"
    );
    let name = format!("bitrouter_shared_{}", uuid::Uuid::new_v4().simple());
    let parameters = target
        .query_pairs()
        .filter(|(key, _)| key != "dbname" && key != "database")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect::<Vec<_>>();
    target.set_query(None);
    if !parameters.is_empty() {
        target.query_pairs_mut().extend_pairs(parameters);
    }
    target.set_path(&name);
    let admin = connect(&source).await?;
    let create = if backend == DatabaseBackend::Postgres {
        format!("CREATE DATABASE \"{name}\" TEMPLATE template0")
    } else {
        format!("CREATE DATABASE `{name}` CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin")
    };
    if !matches!(
        tokio::time::timeout(Duration::from_secs(10), admin.execute_unprepared(&create)).await,
        Ok(Ok(_))
    ) {
        admin.close().await?;
        anyhow::bail!("fixture CREATE unconfirmed; no DROP attempted");
    }
    let mut pools = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        std::panic::AssertUnwindSafe(async {
            let db = connect(target.as_str()).await?;
            pools.push(db.clone());
            let identity = db
                .query_one(Statement::from_string(
                    backend,
                    if backend == DatabaseBackend::Postgres {
                        "SELECT current_database() AS name"
                    } else {
                        "SELECT DATABASE() AS name"
                    },
                ))
                .await?
                .context("database identity")?;
            ensure!(identity.try_get::<String>("", "name")? == name);
            campaign(&db).await?;
            db.close().await?;
            let reopened = connect(target.as_str()).await?;
            pools.push(reopened.clone());
            ensure!(marker(&reopened).await? == 4);
            authority::load_existing(&reopened, chrono::Utc::now()).await?;
            Ok::<_, anyhow::Error>(())
        })
        .catch_unwind(),
    )
    .await;
    let result = result
        .context("server fence deadline")
        .and_then(|r| r.map_err(|_| anyhow::anyhow!("server fence test unwound")))
        .and_then(|r| r);
    let mut cleanup = Ok(());
    for pool in pools {
        if let Err(error) = tokio::time::timeout(Duration::from_secs(10), pool.close())
            .await
            .context("pool cleanup deadline")
            .and_then(|r| r.map_err(Into::into))
        {
            cleanup = Err(error);
        }
    }
    if cleanup.is_ok() {
        let drop_sql = if backend == DatabaseBackend::Postgres {
            format!("DROP DATABASE \"{name}\"")
        } else {
            format!("DROP DATABASE `{name}`")
        };
        cleanup =
            tokio::time::timeout(Duration::from_secs(10), admin.execute_unprepared(&drop_sql))
                .await
                .context("database cleanup deadline")
                .and_then(|r| r.map(|_| ()).map_err(Into::into));
    }
    admin.close().await?;
    result?;
    cleanup?;
    Ok(())
}
