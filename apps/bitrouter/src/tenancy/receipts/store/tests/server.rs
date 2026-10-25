//! Real committed receipt SQL in an acknowledged, isolated server database.
use super::*;
use futures::FutureExt;
use sea_orm::{ConnectOptions, Database, Statement};
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
async fn server_receipts_commit_rollback_reconnect_scope_and_immutable_conflict() -> Result<()> {
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
            crate::db::run_migrations(&db).await?;
            let first = candidate("installation", "tenant", "A", "creator")?;
            let rolled = candidate("installation", "tenant", "R", "creator")?;
            let transaction = db.begin().await?;
            ensure!(insert(&transaction, &rolled).await?);
            transaction.rollback().await?;
            ensure!(
                lookup(
                    &db,
                    &rolled.receipt.installation_id,
                    &rolled.receipt.tenant_id,
                    &rolled.receipt.execution_id
                )
                .await?
                .is_none()
            );
            ensure!(commit(&db, &first).await?);
            ensure!(!commit(&db, &first).await?);
            ensure!(commit(&db, &candidate("installation", "tenant", "B", "creator")?).await?);
            ensure!(
                commit(
                    &db,
                    &candidate("installation", "Tenant", "A", "other-creator")?
                )
                .await?
            );
            ensure!(
                commit(
                    &db,
                    &candidate("installation", "tenant", "A", "changed-creator")?
                )
                .await
                .is_err()
            );
            // A conflicting candidate is explicitly rolled back. The same
            // connection must then perform independent reads and commits.
            let transaction = db.begin().await?;
            let listed = page(
                &transaction,
                &first.receipt.installation_id,
                &first.receipt.tenant_id,
                &first.receipt.correlation_id,
                None,
                1,
            )
            .await?;
            ensure!(listed.multiple_executions && listed.execution_ids == vec!["A".try_into()?]);
            let tail = page(
                &transaction,
                &first.receipt.installation_id,
                &first.receipt.tenant_id,
                &first.receipt.correlation_id,
                listed.next_after.as_ref(),
                1,
            )
            .await?;
            ensure!(
                tail.multiple_executions
                    && tail.execution_ids == vec!["B".try_into()?]
                    && tail.next_after.is_none()
            );
            transaction.rollback().await?;
            let counts = inventory(&db, &first.receipt.installation_id).await?;
            ensure!(counts.rows == 3 && counts.unknown.is_empty());
            let before = lookup(
                &db,
                &first.receipt.installation_id,
                &first.receipt.tenant_id,
                &first.receipt.execution_id,
            )
            .await?
            .context("committed source")?;
            ensure!(serde_json::to_string(&before)? == first.json);
            db.clone().close().await?;
            let reopened = connect(target.as_str()).await?;
            pools.push(reopened.clone());
            let after = lookup(
                &reopened,
                &first.receipt.installation_id,
                &first.receipt.tenant_id,
                &first.receipt.execution_id,
            )
            .await?
            .context("persisted receipt after reconnect")?;
            ensure!(
                serde_json::to_string(&after)? == first.json
                    && after.principal_id.as_str() == "creator"
            );
            ensure!(
                lookup(
                    &reopened,
                    &"other-installation".try_into()?,
                    &first.receipt.tenant_id,
                    &first.receipt.execution_id
                )
                .await?
                .is_none()
            );
            ensure!(
                lookup(
                    &reopened,
                    &first.receipt.installation_id,
                    &"TENANT".try_into()?,
                    &first.receipt.execution_id
                )
                .await?
                .is_none()
            );
            ensure!(bitrouter_ai::tenancy::identity::TenantId::try_from("tenant ").is_err());
            ensure!(!commit(&reopened, &first).await?);
            let mut unknown_receipt =
                candidate("installation", "tenant", "unknown", "creator")?.receipt;
            unknown_receipt.state = ReceiptState::Unknown;
            unknown_receipt.outcome = Outcome::Unknown;
            let unknown = Candidate::new(
                unknown_receipt,
                Disposition {
                    dispatch: DispatchState::Unknown,
                    statuses: Vec::new(),
                },
            )?;
            ensure!(commit(&reopened, &unknown).await?);
            let inventory = inventory(&reopened, &first.receipt.installation_id).await?;
            ensure!(
                inventory.rows == 4
                    && inventory.unknown.len() == 1
                    && inventory.unknown[0].1.as_str() == "unknown"
            );
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
