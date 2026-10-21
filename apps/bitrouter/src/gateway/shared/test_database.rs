//! Test-only ownership of an isolated PostgreSQL database and its client pools.
//!
//! Create this owner outside the campaign timeout, open every in-process pool
//! through `connect`, then await `finish` after the timed future has been dropped.
//! Callers must join their own tasks/children first. There is no detached cleanup
//! or Drop-time SQL; cancelling the owner itself cannot promise cleanup.

use std::time::Duration;

use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};

use super::store;

const ADMIN_TIMEOUT: Duration = Duration::from_secs(10);

/// Only a confirmed CREATE produces this capability. It cannot adopt a database.
pub(super) struct EmptyDatabase {
    admin: DatabaseConnection,
    // Public fixture labels are compatible with existing process campaigns, but
    // neither is authority to redirect DROP or the connection identity check.
    pub(super) name: String,
    pub(super) url: String,
    created_name: String,
    pools: Vec<DatabaseConnection>,
}

impl EmptyDatabase {
    pub(super) async fn create() -> Result<Self> {
        let source = std::env::var("BITROUTER_TEST_DATABASE_URL")
            .context("set BITROUTER_TEST_DATABASE_URL with PostgreSQL CREATEDB privilege")?;
        let name = format!("bitrouter_fixture_{}", uuid::Uuid::new_v4().simple());
        let url = owned_database_url(&source, &name)?;
        let admin = store::connect(&source, 1, Duration::from_secs(3)).await?;
        let prepared = tokio::time::timeout(ADMIN_TIMEOUT, async {
            admin
                .execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    "SET statement_timeout = '10s'".to_owned(),
                ))
                .await?;
            // CREATE DATABASE is outside a transaction, with a generated name.
            // template0 does not copy the admin database's fixture tables.
            // https://www.postgresql.org/docs/current/sql-createdatabase.html
            admin
                .execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    format!("CREATE DATABASE \"{name}\" TEMPLATE template0"),
                ))
                .await?;
            Ok::<(), anyhow::Error>(())
        })
        .await
        .context("fixture database creation deadline exceeded")
        .and_then(|result| result);
        if let Err(error) = prepared {
            // No confirmation means no ownership, including an uncertain CREATE.
            // Never discover/adopt/drop a database after an ambiguous response.
            let closed = close_pool(&admin)
                .await
                .context("closing fixture admin pool after unconfirmed creation");
            return combine_results(
                Err(error.context(format!(
                    "creation of {name} was not confirmed (requires CREATEDB); no DROP attempted"
                ))),
                vec![closed],
            );
        }
        // No await separates confirmed creation from returning the owned handle.
        Ok(Self {
            admin,
            created_name: name.clone(),
            name,
            url,
            pools: Vec::new(),
        })
    }

    /// Retain each pool before another await, including a cancelled identity read.
    /// All DatabaseConnection clones share this pool and its explicit close.
    pub(super) async fn connect(
        &mut self,
        pool_limit: u32,
        timeout: Duration,
    ) -> Result<DatabaseConnection> {
        let db = store::connect(&self.url, pool_limit, timeout).await?;
        self.pools.push(db.clone());
        // Verify the physical database before returning a pool for migration.
        // https://www.postgresql.org/docs/current/functions-info.html
        let identity = tokio::time::timeout(
            timeout,
            db.query_one(Statement::from_string(
                DatabaseBackend::Postgres,
                "SELECT current_database() AS name".to_owned(),
            )),
        )
        .await
        .context("fixture database identity deadline exceeded")??
        .context("missing fixture database identity")?;
        self.verify_name(&identity.try_get::<String>("", "name")?)?;
        Ok(db)
    }

    fn verify_name(&self, actual: &str) -> Result<()> {
        ensure!(
            actual == self.created_name,
            "connection did not reach its fixture-owned database {}",
            self.created_name
        );
        Ok(())
    }

    fn drop_sql(&self) -> String {
        format!("DROP DATABASE \"{}\"", self.created_name)
    }

    /// Close every registered pool, then drop only the confirmed owned database.
    /// The caller awaits this outside its campaign timeout, even on failure.
    pub(super) async fn finish(self, result: Result<()>) -> Result<()> {
        let mut cleanup = Vec::new();
        let mut pools_closed = true;
        for (index, pool) in self.pools.iter().enumerate() {
            let closed = close_pool(pool)
                .await
                .with_context(|| format!("closing fixture pool {index}"));
            pools_closed &= closed.is_ok();
            cleanup.push(closed);
        }
        if pools_closed {
            // No FORCE or backend termination. Remaining foreign/child sessions
            // make DROP fail and are reported, never killed by this capability.
            // https://www.postgresql.org/docs/current/sql-dropdatabase.html
            let dropped = tokio::time::timeout(
                ADMIN_TIMEOUT,
                self.admin.execute(Statement::from_string(
                    DatabaseBackend::Postgres,
                    self.drop_sql(),
                )),
            )
            .await
            .context("fixture database drop deadline exceeded")
            .and_then(|result| result.map(|_| ()).map_err(anyhow::Error::from))
            .with_context(|| format!("dropping fixture-owned database {}", self.created_name));
            cleanup.push(dropped);
        } else {
            cleanup.push(Err(anyhow::anyhow!(
                "database {} retained: DROP skipped because a fixture pool did not close",
                self.created_name
            )));
        }
        cleanup.push(
            close_pool(&self.admin)
                .await
                .context("closing fixture admin pool"),
        );
        combine_results(result, cleanup)
    }
}

async fn close_pool(pool: &DatabaseConnection) -> Result<()> {
    // SQLx Pool::close affects every clone and waits for checked-out connections.
    // A timeout is a failed cleanup, never permission to DROP a still-live pool.
    // https://docs.rs/sqlx/0.8.6/sqlx/struct.Pool.html#method.close
    tokio::time::timeout(ADMIN_TIMEOUT, pool.close_by_ref())
        .await
        .context("fixture pool close deadline exceeded")??;
    Ok(())
}

/// Keep the primary error chain and report every cleanup failure alongside it.
pub(super) fn combine_results<T>(primary: Result<T>, cleanup: Vec<Result<()>>) -> Result<T> {
    let failures: Vec<_> = cleanup
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect();
    if failures.is_empty() {
        return primary;
    }
    let message = format!("fixture cleanup failed:\n{}", failures.join("\n"));
    match primary {
        Err(error) => Err(error.context(message)),
        Ok(_) => Err(anyhow::anyhow!(message)),
    }
}

fn owned_database_url(source: &str, name: &str) -> Result<String> {
    ensure!(
        !name.is_empty()
            && name.len() <= 63
            && name
                .bytes()
                .all(|byte| { byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_' }),
        "invalid generated fixture database name"
    );
    let mut url = url::Url::parse(source)?;
    let parameters: Vec<_> = url
        .query_pairs()
        .filter(|(key, _)| key != "dbname")
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    url.set_query(None);
    if !parameters.is_empty() {
        url.query_pairs_mut().extend_pairs(parameters);
    }
    url.set_path(name);
    Ok(url.into())
}

#[test]
fn fixture_database_cannot_be_overridden_by_source_query() -> Result<()> {
    let url = url::Url::parse(&owned_database_url(
        "postgres://fixture:secret@localhost/admin?dbname=original&db%6eame=second&%64bname=third&sslmode=disable&host=%2Ftmp&options=-c%20statement_timeout%3D5000",
        "owned_fixture",
    )?)?;
    ensure!(url.path() == "/owned_fixture");
    ensure!(url.query_pairs().all(|(key, _)| key != "dbname"));
    let parameters: Vec<_> = url
        .query_pairs()
        .map(|(key, value)| (key.into_owned(), value.into_owned()))
        .collect();
    ensure!(
        parameters
            == vec![
                ("sslmode".into(), "disable".into()),
                ("host".into(), "/tmp".into()),
                ("options".into(), "-c statement_timeout=5000".into()),
            ]
    );
    ensure!(url.username() == "fixture" && url.password() == Some("secret"));
    Ok(())
}

#[test]
fn display_fields_cannot_redirect_owned_database_cleanup() -> Result<()> {
    // A pure capability check: Disconnected performs no database operations.
    let created_name = "bitrouter_fixture_0123456789abcdef0123456789abcdef";
    let mut database = EmptyDatabase {
        admin: DatabaseConnection::Disconnected,
        created_name: created_name.into(),
        name: created_name.into(),
        url: "postgres://localhost/owned".into(),
        pools: Vec::new(),
    };
    database.name = "postgres".into();
    database.url = "postgres://elsewhere/postgres?dbname=foreign".into();
    ensure!(database.drop_sql() == format!("DROP DATABASE \"{created_name}\""));
    database.verify_name(created_name)?;
    ensure!(database.verify_name("postgres").is_err());
    ensure!(database.verify_name("").is_err());
    Ok(())
}

#[test]
fn cleanup_failure_keeps_primary_and_every_cleanup_error() -> Result<()> {
    let error = combine_results::<()>(
        Err(anyhow::anyhow!("campaign failed")),
        vec![
            Err(anyhow::anyhow!("first pool failed")),
            Ok(()),
            Err(anyhow::anyhow!("database drop failed")),
        ],
    )
    .err()
    .context("cleanup erased the primary failure")?;
    let message = format!("{error:#}");
    for expected in [
        "campaign failed",
        "first pool failed",
        "database drop failed",
    ] {
        ensure!(message.contains(expected), "lost failure: {expected}");
    }
    ensure!(combine_results(Ok(()), vec![Err(anyhow::anyhow!("cleanup only"))]).is_err());
    ensure!(combine_results::<()>(Err(anyhow::anyhow!("primary only")), vec![Ok(())]).is_err());
    combine_results(Ok(()), vec![Ok(())])
}
