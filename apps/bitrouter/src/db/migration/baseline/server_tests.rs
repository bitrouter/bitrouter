//! Real isolated PostgreSQL/MySQL baseline and refusal campaigns; no old binary.
use super::*;
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectOptions, Database, DatabaseConnection};

struct OwnedServer {
    admin: DatabaseConnection,
    database: DatabaseConnection,
    name: String,
}

impl OwnedServer {
    async fn new(backend: DatabaseBackend) -> Result<Self> {
        let address = std::env::var("BITROUTER_TEST_MIGRATION_DATABASE_URL")
            .context("dedicated migration database URL required; no fallback")?;
        let mut selected = url::Url::parse(&address).context("invalid dedicated database URL")?;
        ensure!(
            match backend {
                DatabaseBackend::Postgres => matches!(selected.scheme(), "postgres" | "postgresql"),
                DatabaseBackend::MySql => selected.scheme() == "mysql",
                _ => false,
            },
            "wrong backend selected for baseline campaign"
        );
        let mut options = ConnectOptions::new(address);
        options
            .max_connections(1)
            .min_connections(1)
            .sqlx_logging(false);
        let admin = Database::connect(options)
            .await
            .context("baseline admin connection unavailable")?;
        let name = format!("m0_baseline_{}", uuid::Uuid::new_v4().simple());
        let create = match backend {
            DatabaseBackend::Postgres => format!("CREATE DATABASE {name}"),
            DatabaseBackend::MySql => {
                format!("CREATE DATABASE {name} CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_bin")
            }
            _ => anyhow::bail!("server campaign backend"),
        };
        admin.execute_unprepared(&create).await?;
        selected.set_path(&format!("/{name}"));
        let mut options = ConnectOptions::new(selected.as_str());
        options
            .max_connections(1)
            .min_connections(1)
            .sqlx_logging(false);
        let database = match Database::connect(options).await {
            Ok(database) => database,
            Err(error) => {
                admin
                    .execute_unprepared(&format!("DROP DATABASE {name}"))
                    .await?;
                admin.close().await?;
                return Err(error.into());
            }
        };
        Ok(Self {
            admin,
            database,
            name,
        })
    }

    async fn finish(self, result: Result<()>) -> Result<()> {
        let closed = self.database.close().await;
        let dropped = self
            .admin
            .execute_unprepared(&format!("DROP DATABASE {}", self.name))
            .await;
        let admin_closed = self.admin.close().await;
        result?;
        closed?;
        dropped?;
        admin_closed?;
        Ok(())
    }
}

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
    let fixture = OwnedServer::new(backend).await?;
    let checked = async {
        let db = &fixture.database;
        ensure!(store_format::inspect(db).await? == State::Empty);
        super::super::Migrator::up(db, None).await?;
        // This compares the real backend catalog with the frozen, independently
        // reviewed schema profile, including keys, collation and constraints.
        crate::db::current_schema::validate(db).await?;
        let ledger = super::super::Migrator::get_migration_models(db).await?;
        ensure!(ledger.len() == 1 && ledger[0].version == store_format::BASELINE);
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
            "ALTER TABLE tenancy_dispatch_intents ENGINE=MyISAM".to_owned(),
            "CREATE TRIGGER unexpected_intent_trigger BEFORE INSERT ON tenancy_dispatch_intents FOR EACH ROW SET NEW.byte_count = NEW.byte_count".to_owned(),
        ]),
        _ => anyhow::bail!("server campaign backend"),
    }
    for corruption in corruptions {
        let fixture = OwnedServer::new(backend).await?;
        let checked = async {
            let db = &fixture.database;
            super::super::Migrator::up(db, None).await?;
            let ledger = super::super::Migrator::get_migration_models(db).await?;
            db.execute_unprepared(&corruption).await?;
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
        let fixture = OwnedServer::new(backend).await?;
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
