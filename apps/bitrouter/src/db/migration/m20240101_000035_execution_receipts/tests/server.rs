//! Opt-in backend DDL checks create and remove only their acknowledged database.
use super::*;
use anyhow::Context;
use futures::FutureExt;
use sea_orm::sea_query::Expr;
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
        .map_err(|_| anyhow::anyhow!("receipt backend fixture connection failed"))
}
#[tokio::test]
#[ignore = "requires BITROUTER_TEST_DATABASE_URL with PG/MySQL CREATE/DROP DATABASE privileges"]
async fn server_receipt035_exact_indexes_persistent_restart_and_populated_down() -> Result<()> {
    let source = std::env::var("BITROUTER_TEST_DATABASE_URL")
        .context("dedicated PG/MySQL test URL required")?;
    let mut target = url::Url::parse(&source)?;
    let backend = match target.scheme() {
        "postgres" | "postgresql" => {
            ensure!(cfg!(feature = "db-postgres"), "enable db-postgres");
            DatabaseBackend::Postgres
        }
        "mysql" => {
            ensure!(cfg!(feature = "db-mysql"), "enable db-mysql");
            DatabaseBackend::MySql
        }
        _ => anyhow::bail!("PG/MySQL fixture required"),
    };
    ensure!(
        !target.path().trim_matches('/').is_empty(),
        "explicit admin database required"
    );
    let name = format!("bitrouter_eval_receipt_{}", uuid::Uuid::new_v4().simple());
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
    // A failed/unconfirmed CREATE grants no cleanup authority.
    let created = tokio::time::timeout(
        Duration::from_secs(10),
        admin.execute(Statement::from_string(backend, create)),
    )
    .await;
    if !matches!(created, Ok(Ok(_))) {
        admin.close().await?;
        anyhow::bail!("receipt fixture CREATE unconfirmed; no DROP attempted");
    }
    let mut pools = Vec::new();
    let result = tokio::time::timeout(
        Duration::from_secs(120),
        std::panic::AssertUnwindSafe(async {
            let db = connect(target.as_str()).await?;
            pools.push(db.clone());
            let actual = db
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
            ensure!(
                actual.try_get::<String>("", "name")? == name,
                "fixture escaped owned database"
            );
            let manager = SchemaManager::new(&db);
            Migration.up(&manager).await?;
            seed(&db).await?;
            let before = saved(&db).await?;
            Migration.up(&manager).await?;
            ensure!(saved(&db).await? == before);
            if backend == DatabaseBackend::Postgres {
                db.execute_unprepared("ALTER TABLE tenancy_execution_receipts SET UNLOGGED")
                    .await?;
                ensure!(Migration.up(&manager).await.is_err());
                ensure!(saved(&db).await? == before);
                db.execute_unprepared("ALTER TABLE tenancy_execution_receipts SET LOGGED")
                    .await?;
            }
            manager
                .drop_index(
                    Index::drop()
                        .name(INDEX)
                        .table(Alias::new(TABLE))
                        .to_owned(),
                )
                .await?;
            Migration.up(&manager).await?;
            ensure!(saved(&db).await? == before);
            ensure!(Migration.down(&manager).await.is_err());
            ensure!(saved(&db).await? == before);
            db.clone().close().await?;
            let reopened = connect(target.as_str()).await?;
            pools.push(reopened.clone());
            Migration.up(&SchemaManager::new(&reopened)).await?;
            ensure!(saved(&reopened).await? == before);
            // Compare case and trailing spaces using the actual identity indexes.
            for tenant in ["tenant", "Tenant "] {
                let query = Query::select()
                    .expr(Expr::val(1))
                    .from(Alias::new(TABLE))
                    .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant))
                    .to_owned();
                ensure!(reopened.query_one(backend.build(&query)).await?.is_none());
            }
            Ok::<_, anyhow::Error>(())
        })
        .catch_unwind(),
    )
    .await;
    let result = result
        .context("receipt backend campaign deadline")
        .and_then(|result| result.map_err(|_| anyhow::anyhow!("receipt backend campaign unwound")))
        .and_then(|result| result);
    // Always close our pools before DROP; no FORCE or foreign-session termination.
    let mut cleanup = Ok(());
    for pool in pools {
        if let Err(error) = tokio::time::timeout(Duration::from_secs(10), pool.close())
            .await
            .context("receipt pool cleanup deadline")
            .and_then(|result| result.map_err(Into::into))
        {
            cleanup = Err(error);
        }
    }
    if cleanup.is_ok() {
        let drop = if backend == DatabaseBackend::Postgres {
            format!("DROP DATABASE \"{name}\"")
        } else {
            format!("DROP DATABASE `{name}`")
        };
        cleanup = tokio::time::timeout(
            Duration::from_secs(10),
            admin.execute(Statement::from_string(backend, drop)),
        )
        .await
        .context("receipt database cleanup deadline")
        .and_then(|result| result.map(|_| ()).map_err(Into::into));
    }
    let closed = admin.close().await;
    cleanup?;
    closed?;
    result
}
