//! Real isolated PostgreSQL/MySQL baseline and refusal campaigns; no old binary.
use super::super::server_fixture::OwnedServer;
use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::DatabaseConnection;

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires dedicated PostgreSQL CREATEDB; no fallback"]
async fn postgres_current_baseline_catalog_and_refusal() -> Result<()> {
    campaign(DatabaseBackend::Postgres).await
}

#[cfg(feature = "db-mysql")]
#[tokio::test]
#[ignore = "requires dedicated MySQL CREATE DATABASE; no fallback"]
async fn mysql_current_baseline_catalog_and_refusal() -> Result<()> {
    campaign(DatabaseBackend::MySql).await
}

async fn campaign(backend: DatabaseBackend) -> Result<()> {
    let fixture = OwnedServer::new(backend, "BITROUTER_TEST_MIGRATION_DATABASE_URL").await?;
    let checked = async {
        let db = &fixture.database;
        ensure!(store_format::inspect(db).await? == State::Empty);
        super::super::Migrator::up(db, None).await?;
        // This compares the real backend catalog with the frozen, independently
        // reviewed schema profile, including keys, collation and constraints.
        crate::db::current_schema::validate(db).await?;
        for table in [
            "acp_capture_connections",
            "acp_canonical_sessions",
            "bro_runtime_ownership",
            "bro_execution_records",
        ] {
            ensure!(
                !SchemaManager::new(db).has_table(table).await?,
                "unexpected historical table {table}"
            );
        }
        let ledger = super::super::Migrator::get_migration_models(db).await?;
        ensure!(ledger.len() == 1 && ledger[0].version == store_format::GATEWAY_SERVER);
        db.execute_unprepared(
            "INSERT INTO users(id,created_at) VALUES ('retained-user','2026-11-18T00:00:00Z')",
        )
        .await?;
        for _ in 0..2 {
            super::super::Migrator::up(db, None).await?;
            ensure!(super::super::Migrator::get_migration_models(db).await? == ledger);
            ensure!(
                db.query_one(Statement::from_string(
                    backend,
                    "SELECT id FROM users WHERE id='retained-user'"
                ))
                .await?
                .is_some()
            );
        }
        if backend == DatabaseBackend::MySql {
            mysql_metering_restart(&fixture).await?;
        }
        let drop_index = match backend {
            DatabaseBackend::Postgres => "DROP INDEX idx_requests_user_created",
            _ => "DROP INDEX idx_requests_user_created ON requests",
        };
        db.execute_unprepared(drop_index).await?;
        ensure!(
            super::super::Migrator::up(db, None).await.is_err(),
            "missing current index was adopted"
        );
        ensure!(super::super::Migrator::get_migration_models(db).await? == ledger);
        ensure!(
            db.query_one(Statement::from_string(
                backend,
                "SELECT id FROM users WHERE id='retained-user'"
            ))
            .await?
            .is_some()
        );
        Ok(())
    }
    .await;
    fixture.finish(checked).await?;

    let fixture = OwnedServer::new(backend, "BITROUTER_TEST_MIGRATION_DATABASE_URL").await?;
    let checked = async {
        let db = &fixture.database;
        super::super::m0_fixture::install(db).await?;
        if backend == DatabaseBackend::MySql {
            let original = db.query_one(Statement::from_string(backend,
                "SELECT COLLATION_NAME AS comparison FROM information_schema.COLUMNS WHERE TABLE_SCHEMA=DATABASE() AND TABLE_NAME='requests' AND COLUMN_NAME='request_id'"))
                .await?.context("missing M0 request ID")?;
            ensure!(original.try_get::<String>("", "comparison")? == "utf8mb4_0900_ai_ci");
        }
        db.execute_unprepared("INSERT INTO users(id,created_at) VALUES ('historical-user','2026-11-18T00:00:00Z')").await?;
        let before = super::super::Migrator::get_migration_models(db).await?;
        ensure!(before.len()==1 && before[0].version==store_format::BASELINE);
        let catalog = match backend {
            DatabaseBackend::Postgres => "SELECT table_name FROM information_schema.tables WHERE table_schema=current_schema() ORDER BY table_name",
            _ => "SELECT TABLE_NAME AS table_name FROM information_schema.tables WHERE TABLE_SCHEMA=DATABASE() ORDER BY TABLE_NAME",
        };
        let tables = || async {
            db.query_all(Statement::from_string(backend, catalog)).await?.into_iter().map(|row| row.try_get::<String>("","table_name")).collect::<Result<Vec<_>, DbErr>>()
        };
        let original_tables = tables().await?;
        for _ in 0..2 {
            let error = crate::db::connect(&fixture.url).await.err().context("mixed server connected")?;
            ensure!(error.to_string().contains("new empty database"), "{error}");
            let error = crate::db::run_migrations(db).await.err().context("mixed server accepted")?;
            ensure!(tables().await? == original_tables);
            ensure!(error.to_string().contains("new empty database"), "{error}");
            ensure!(super::super::Migrator::get_migration_models(db).await?==before);
            ensure!(db.query_one(Statement::from_string(backend,"SELECT id FROM users WHERE id='historical-user'")).await?.is_some());
            ensure!(db.query_all(Statement::from_string(backend,"SELECT connection_id FROM acp_capture_connections LIMIT 1")).await?.is_empty());
        }
        Ok(())
    }.await;
    fixture.finish(checked).await?;

    if backend == DatabaseBackend::Postgres {
        postgres_unvalidated_not_null_refusal().await?;
    }

    let mut corruptions = vec![
        "ALTER TABLE tenancy_execution_receipts ALTER COLUMN receipt_format SET DEFAULT 2".to_owned(),
        "ALTER TABLE tenancy_execution_receipts DROP COLUMN receipt_format".to_owned(),
        "ALTER TABLE tenancy_dispatch_intents DROP COLUMN intent_json".to_owned(),
        "CREATE UNIQUE INDEX unexpected_usage_identity ON tenancy_usage_evidence (installation_id,tenant_id,execution_id)".to_owned(),
        "ALTER TABLE tenancy_dispatch_intents ADD CONSTRAINT unexpected_byte_check CHECK (byte_count >= 0)".to_owned(),
    ];
    match backend {
        DatabaseBackend::Postgres => corruptions.extend([
            "ALTER TABLE tenancy_dispatch_intents DROP CONSTRAINT tenancy_dispatch_intents_pkey".to_owned(),
            "ALTER TABLE tenancy_dispatch_intents ALTER COLUMN tenant_id TYPE varchar(128) COLLATE \"default\"".to_owned(),
            "ALTER TABLE tenancy_dispatch_intents ENABLE ROW LEVEL SECURITY".to_owned(),
            "CREATE FUNCTION unexpected_intent_trigger() RETURNS trigger LANGUAGE plpgsql AS $$BEGIN RETURN NEW; END$$; CREATE TRIGGER unexpected_intent_trigger BEFORE INSERT ON tenancy_dispatch_intents FOR EACH ROW EXECUTE FUNCTION unexpected_intent_trigger()".to_owned(),
        ]),
        DatabaseBackend::MySql => corruptions.extend([
            "ALTER TABLE tenancy_dispatch_intents DROP PRIMARY KEY".to_owned(),
            "ALTER TABLE tenancy_dispatch_intents MODIFY tenant_id varchar(128) CHARACTER SET utf8mb4 COLLATE utf8mb4_general_ci NOT NULL".to_owned(),
            // The integer primary key fits MyISAM's index limit, so the
            // catalog validator gets to reject the nontransactional engine.
            "ALTER TABLE provider_continuation_key_epoch ENGINE=MyISAM".to_owned(),
            "CREATE TRIGGER unexpected_intent_trigger BEFORE INSERT ON tenancy_dispatch_intents FOR EACH ROW SET NEW.byte_count = NEW.byte_count".to_owned(),
        ]),
        _ => anyhow::bail!("server campaign backend"),
    }
    for corruption in corruptions {
        let fixture = OwnedServer::new(backend, "BITROUTER_TEST_MIGRATION_DATABASE_URL").await?;
        let checked = async {
            let db = &fixture.database;
            super::super::Migrator::up(db, None).await?;
            let ledger = super::super::Migrator::get_migration_models(db).await?;
            db.execute_unprepared(&corruption)
                .await
                .with_context(|| format!("applying catalog fixture: {corruption}"))?;
            ensure!(
                super::super::Migrator::up(db, None).await.is_err(),
                "current catalog accepted {corruption}"
            );
            ensure!(super::super::Migrator::get_migration_models(db).await? == ledger);
            Ok(())
        }
        .await;
        fixture.finish(checked).await?;
    }

    for marker in [
        None,
        Some("m20240101_000048_native_generate_content"),
        Some("m20990101_future"),
    ] {
        let fixture = OwnedServer::new(backend, "BITROUTER_TEST_MIGRATION_DATABASE_URL").await?;
        let checked = async {
            let db = &fixture.database;
            db.execute_unprepared("CREATE TABLE preserved(value VARCHAR(128) NOT NULL)").await?;
            db.execute_unprepared("INSERT INTO preserved VALUES ('unchanged')").await?;
            if let Some(marker) = marker {
                db.execute_unprepared("CREATE TABLE seaql_migrations(version VARCHAR(255) PRIMARY KEY,applied_at BIGINT NOT NULL)").await?;
                db.execute(Statement::from_sql_and_values(backend,
                    match backend {DatabaseBackend::Postgres=>"INSERT INTO seaql_migrations VALUES ($1,1)",_=>"INSERT INTO seaql_migrations VALUES (?,1)"},[marker.into()])).await?;
            }
            ensure!(super::super::Migrator::up(db,None).await.is_err());
            ensure!(!SchemaManager::new(db).has_table("tenancy_installation").await?);
            ensure!(SchemaManager::new(db).has_table("seaql_migrations").await?==marker.is_some());
            ensure!(db.query_one(Statement::from_string(backend,
                "SELECT value FROM preserved WHERE value='unchanged'")).await?.is_some());
            Ok(())
        }.await;
        fixture.finish(checked).await?;
    }
    Ok(())
}

async fn postgres_unvalidated_not_null_refusal() -> Result<()> {
    let fixture = OwnedServer::new(
        DatabaseBackend::Postgres,
        "BITROUTER_TEST_MIGRATION_DATABASE_URL",
    )
    .await?;
    let checked = async {
        let db = &fixture.database;
        let version = db
            .query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT current_setting('server_version_num')::integer AS version",
            ))
            .await?
            .context("PostgreSQL version")?
            .try_get::<i32>("", "version")?;
        if version < 180_000 {
            return Ok(());
        }
        super::super::Migrator::up(db, None).await?;
        db.execute(Statement::from_sql_and_values(
            DatabaseBackend::Postgres,
            "INSERT INTO tenancy_installation(singleton,installation_id,schema_version,authority_revision,operation_ids,safety_operation_ids,saturated,created_at) VALUES(1,'not-null-owner',$1,7,0,0,false,'2026-11-18T00:00:00Z')",
            [crate::tenancy::AUTHORITY_SCHEMA_VERSION.into()],
        )).await?;
        // PG18 retains attnotnull=true even while existing NULL rows have not
        // been validated. A column-flag-only validator would accept this shape.
        db.execute_unprepared(
            "ALTER TABLE users ALTER COLUMN created_at DROP NOT NULL;
             INSERT INTO users(id,created_at) VALUES ('unvalidated-user',NULL);
             ALTER TABLE users ADD CONSTRAINT fixture_created_at_not_null NOT NULL created_at NOT VALID",
        ).await?;
        let state = db.query_one(Statement::from_string(DatabaseBackend::Postgres,
            "SELECT a.attnotnull,k.convalidated FROM pg_attribute a JOIN pg_class t ON t.oid=a.attrelid JOIN pg_namespace n ON n.oid=t.relnamespace JOIN pg_constraint k ON k.conrelid=t.oid AND a.attnum=ANY(k.conkey) WHERE n.nspname=current_schema() AND t.relname='users' AND a.attname='created_at' AND k.contype='n'"
        )).await?.context("not-null constraint")?;
        ensure!(state.try_get::<bool>("", "attnotnull")?);
        ensure!(!state.try_get::<bool>("", "convalidated")?);
        let ledger = super::super::Migrator::get_migration_models(db).await?;
        let authority = postgres_authority_snapshot(db).await?;
        let error = super::super::Migrator::up(db, None)
            .await
            .err()
            .context("unvalidated not-null constraint selected a current dataset")?;
        ensure!(error.to_string().contains("unsupported table storage or mutation hooks: users"));
        ensure!(super::super::Migrator::get_migration_models(db).await? == ledger);
        ensure!(postgres_authority_snapshot(db).await? == authority);
        let row = db.query_one(Statement::from_string(DatabaseBackend::Postgres,
            "SELECT created_at FROM users WHERE id='unvalidated-user'"
        )).await?.context("retained NULL row disappeared")?;
        ensure!(row.try_get::<Option<String>>("", "created_at")?.is_none());
        // A repaired and validated not-null constraint remains supported.
        db.execute_unprepared(
            "DELETE FROM users WHERE id='unvalidated-user';
             ALTER TABLE users VALIDATE CONSTRAINT fixture_created_at_not_null",
        ).await?;
        super::super::Migrator::up(db, None).await?;
        ensure!(super::super::Migrator::get_migration_models(db).await? == ledger);
        ensure!(postgres_authority_snapshot(db).await? == authority);
        Ok(())
    }.await;
    fixture.finish(checked).await
}

async fn postgres_authority_snapshot(db: &DatabaseConnection) -> Result<String> {
    db.query_one(Statement::from_string(
        DatabaseBackend::Postgres,
        "SELECT row_to_json(i)::text AS authority FROM tenancy_installation i",
    ))
    .await?
    .context("authority snapshot")?
    .try_get("", "authority")
    .map_err(Into::into)
}

/// Earlier gateways could inherit case/accent folding or PAD SPACE semantics.
/// Refusal must preserve the real settlement and the already-issued marker.
async fn mysql_metering_restart(fixture: &OwnedServer) -> Result<()> {
    let db = &fixture.database;
    db.execute_unprepared("INSERT INTO requests(request_id,user_id,api_key_id,model_id,provider_id,prompt_tokens,completion_tokens,reasoning_tokens,estimated_charge_micro_usd,created_at) VALUES('Paid-A','Owner','Key','model','provider',11,3,0,17,'2026-11-18T00:00:00Z')").await?;
    let original = super::super::Migrator::get_migration_models(db).await?;
    for (name, comparison) in [
        ("request_id", "utf8mb4_0900_ai_ci"),
        ("api_key_id", "utf8mb4_bin"),
    ] {
        db.execute_unprepared(&format!("ALTER TABLE requests MODIFY {name} VARCHAR(255) CHARACTER SET utf8mb4 COLLATE {comparison} NOT NULL")).await?;
        let error = crate::db::connect(&fixture.url)
            .await
            .err()
            .context("gateway opened incompatible metering catalog")?;
        ensure!(
            error
                .to_string()
                .contains(&format!("requests.{name} requires utf8mb4_0900_bin")),
            "{error}"
        );
        ensure!(crate::db::run_migrations(db).await.is_err());
        ensure!(super::super::Migrator::get_migration_models(db).await? == original);
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::MySql,
                "SELECT request_id,user_id,api_key_id,estimated_charge_micro_usd FROM requests",
            ))
            .await?
            .context("settlement missing after refusal")?;
        ensure!(
            row.try_get::<String>("", "request_id")? == "Paid-A"
                && row.try_get::<String>("", "user_id")? == "Owner"
                && row.try_get::<String>("", "api_key_id")? == "Key"
                && row.try_get::<i64>("", "estimated_charge_micro_usd")? == 17
        );
        db.execute_unprepared(&format!("ALTER TABLE requests MODIFY {name} VARCHAR(255) CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin NOT NULL")).await?;
    }
    crate::db::connect(&fixture.url).await?.close().await?;
    Ok(())
}
