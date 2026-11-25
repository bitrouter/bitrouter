//! First-identity probes distinguish passive names from retained ownership.
use super::*;
use sea_orm::{DatabaseConnection, DbErr, ExecResult};

async fn probe_cases(db: &DatabaseConnection) -> Result<()> {
    crate::db::run_migrations(db).await?;
    ensure_no_owned_rows(db).await?;
    ensure!(!available(db).await?);

    // Neither a lookalike payload column nor an occupied stamp name registers
    // capability. Strict readers/audits must still refuse these objects.
    db.execute_unprepared("CREATE TABLE tenancy_provider_technical_facts(retain TEXT,technical_json TEXT); CREATE TABLE tenancy_provider_technical_schema(retain TEXT)").await?;
    ensure_no_owned_rows(db).await?;
    db.execute_unprepared("INSERT INTO tenancy_provider_technical_facts VALUES('unknown-object','not registered'); INSERT INTO tenancy_provider_technical_schema VALUES('unknown-stamp')").await?;
    ensure_no_owned_rows(db).await?;
    ensure!(available(db).await.is_err());
    db.execute_unprepared(
        "DROP TABLE tenancy_provider_technical_schema; DROP TABLE tenancy_provider_technical_facts",
    )
    .await?;

    // A single ownership column is sufficient. Neither malformed values nor
    // NULL provenance may turn a retained row into absence. SQLite identifiers
    // are case insensitive, so include a quoted uppercase spelling as well.
    for name in [
        "installation_id",
        "tenant_id",
        "execution_id",
        "INSTALLATION_ID",
    ] {
        let mut table = Table::create();
        table
            .table(Alias::new(TABLE))
            .col(ColumnDef::new(Alias::new(name)).text());
        db.execute(db.get_database_backend().build(&table)).await?;
        ensure_no_owned_rows(db).await?;
        let row = Query::insert()
            .into_table(Alias::new(TABLE))
            .columns([Alias::new(name)])
            .values([Option::<String>::None.into()])?
            .to_owned();
        db.execute(db.get_database_backend().build(&row)).await?;
        ensure!(ensure_no_owned_rows(db).await.is_err());
        ensure!(available(db).await.is_err());
        db.execute_unprepared("DROP TABLE tenancy_provider_technical_facts")
            .await?;
    }

    install(db).await?;
    ensure!(available(db).await?);
    ensure_no_owned_rows(db).await?;
    // The real registered table retains a receipt FK. Construct an orphaned
    // authority with that link intact, as after losing its installation header.
    let receipt: [(&str, sea_orm::Value); 11] = [
        ("installation_id", "retained-installation".into()),
        ("tenant_id", "retained-tenant".into()),
        ("execution_id", "retained-execution".into()),
        ("principal_id", "retained-principal".into()),
        ("correlation_id", "retained-correlation".into()),
        ("receipt_json", "{}".into()),
        ("content_digest", "0".repeat(64).into()),
        ("disposition_json", "{}".into()),
        ("byte_count", 4_i64.into()),
        ("unknown_outcome", false.into()),
        ("recorded_at", "2026-11-18T00:00:00Z".into()),
    ];
    let source = Query::insert()
        .into_table(Alias::new("tenancy_execution_receipts"))
        .columns(receipt.iter().map(|(name, _)| Alias::new(*name)))
        .values(receipt.into_iter().map(|(_, value)| value.into()))?
        .to_owned();
    db.execute(db.get_database_backend().build(&source)).await?;
    let technical: [(&str, sea_orm::Value); 8] = [
        ("installation_id", "retained-installation".into()),
        ("tenant_id", "retained-tenant".into()),
        ("execution_id", "retained-execution".into()),
        ("schema_version", 1_i32.into()),
        ("receipt_digest", "0".repeat(64).into()),
        ("technical_digest", "1".repeat(64).into()),
        ("technical_json", "{}".into()),
        ("byte_count", 2_i64.into()),
    ];
    let source = Query::insert()
        .into_table(Alias::new(TABLE))
        .columns(technical.iter().map(|(name, _)| Alias::new(*name)))
        .values(technical.into_iter().map(|(_, value)| value.into()))?
        .to_owned();
    db.execute(db.get_database_backend().build(&source)).await?;
    ensure!(available(db).await?);
    ensure!(ensure_no_owned_rows(db).await.is_err());
    // An empty temporary namesake must never mask retained rows in the formal
    // namespace. The helper remains a refusal check, not capability validation.
    db.execute_unprepared(
        "CREATE TEMP TABLE tenancy_provider_technical_facts(installation_id TEXT)",
    )
    .await?;
    ensure!(ensure_no_owned_rows(db).await.is_err());
    db.execute_unprepared("DROP TABLE tenancy_provider_technical_facts")
        .await?;

    for catalog in [true, false] {
        let fault = FailedProbe { db, catalog };
        let error = ensure_no_owned_rows(&fault)
            .await
            .err()
            .context("SQL failure was ignored")?;
        ensure!(
            error
                .to_string()
                .contains("injected identity probe SQL failure")
        );
    }
    Ok(())
}

struct FailedProbe<'a> {
    db: &'a DatabaseConnection,
    catalog: bool,
}

#[async_trait::async_trait]
impl ConnectionTrait for FailedProbe<'_> {
    fn get_database_backend(&self) -> DatabaseBackend {
        self.db.get_database_backend()
    }
    async fn execute(&self, statement: Statement) -> std::result::Result<ExecResult, DbErr> {
        self.db.execute(statement).await
    }
    async fn execute_unprepared(&self, sql: &str) -> std::result::Result<ExecResult, DbErr> {
        self.db.execute_unprepared(sql).await
    }
    async fn query_one(
        &self,
        statement: Statement,
    ) -> std::result::Result<Option<QueryResult>, DbErr> {
        let catalog =
            statement.sql.contains("pragma_table_xinfo") || statement.sql.contains("pg_attribute");
        if self.catalog == catalog {
            return Err(DbErr::Custom("injected identity probe SQL failure".into()));
        }
        self.db.query_one(statement).await
    }
    async fn query_all(
        &self,
        statement: Statement,
    ) -> std::result::Result<Vec<QueryResult>, DbErr> {
        self.db.query_all(statement).await
    }
}

#[tokio::test]
async fn sqlite_identity_probe_preserves_passive_names_and_refuses_owned_rows() -> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    let checked = probe_cases(&db).await;
    let closed = db.close().await;
    checked?;
    closed?;
    Ok(())
}

#[cfg(feature = "db-postgres")]
#[tokio::test]
#[ignore = "requires dedicated PostgreSQL CREATEDB; no fallback"]
async fn postgres_identity_probe_preserves_passive_names_and_refuses_owned_rows() -> Result<()> {
    let fixture = crate::db::migration::server_fixture::OwnedServer::new(
        DatabaseBackend::Postgres,
        "BITROUTER_TEST_MIGRATION_DATABASE_URL",
    )
    .await?;
    let checked = probe_cases(&fixture.database).await;
    fixture.finish(checked).await
}

#[tokio::test]
async fn sqlite_passive_optional_names_allow_actual_first_service_and_remain_unregistered()
-> Result<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    db.execute_unprepared("CREATE TABLE tenancy_provider_technical_facts(retain TEXT,technical_json TEXT); CREATE TABLE tenancy_provider_technical_schema(retain TEXT); INSERT INTO tenancy_provider_technical_facts VALUES('unknown-object','not registered'); INSERT INTO tenancy_provider_technical_schema VALUES('unknown-stamp')").await?;
    let service = crate::tenancy::Service::open(db.clone(), None).await?;
    let checked: Result<()> = async {
        ensure!(
            db.query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT installation_id FROM tenancy_installation WHERE singleton=1"
            ))
            .await?
            .is_some()
        );
        ensure!(available(&db).await.is_err());
        // Creating an identity does not adopt the unknown object or its bytes.
        let inventory = crate::db::migration::capacity::load(&db).await?;
        ensure!(inventory.technical_bytes == 0);
        Ok(())
    }
    .await;
    let stopped = service.shutdown().await;
    drop(service);
    let closed = db.close().await;
    checked?;
    stopped?;
    closed?;
    Ok(())
}

#[tokio::test]
async fn sqlite_first_service_refuses_partial_owned_rows_even_with_zero_summary() -> Result<()> {
    for column in ["installation_id", "tenant_id", "execution_id"] {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let mut table = Table::create();
        table
            .table(Alias::new(TABLE))
            .col(ColumnDef::new(Alias::new(column)).text());
        db.execute(DatabaseBackend::Sqlite.build(&table)).await?;
        let row = Query::insert()
            .into_table(Alias::new(TABLE))
            .columns([Alias::new(column)])
            .values([Option::<String>::None.into()])?
            .to_owned();
        db.execute(DatabaseBackend::Sqlite.build(&row)).await?;
        match crate::tenancy::Service::open(db.clone(), None).await {
            Ok(service) => {
                service.shutdown().await?;
                anyhow::bail!("created a new authority over a partial owned row");
            }
            Err(error) => {
                ensure!(format!("{error:#}").contains("authority has no installation identity"))
            }
        }
        ensure!(
            db.query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT singleton FROM tenancy_installation"
            ))
            .await?
            .is_none()
        );
        ensure!(
            crate::db::migration::capacity::load(&db)
                .await?
                .technical_bytes
                == 0
        );
        db.close().await?;
    }
    Ok(())
}
