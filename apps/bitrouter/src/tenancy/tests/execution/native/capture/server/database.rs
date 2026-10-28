//! Own exactly one disposable backend DB and every Service/pool outside test timeouts.
use crate::tenancy::Service;
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectOptions, Database};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use std::time::Duration;

const CLEANUP: Duration = Duration::from_secs(10);

pub(in crate::tenancy::tests::execution::native::capture) struct Fixture {
    admin: DatabaseConnection,
    backend: DatabaseBackend,
    name: String,
    url: String,
    pools: Vec<DatabaseConnection>,
    services: Vec<Service>,
}

impl Fixture {
    pub(in crate::tenancy::tests::execution::native::capture) async fn create() -> Result<Self> {
        let source = std::env::var("BITROUTER_TEST_DATABASE_URL")
            .context("set a dedicated PG/MySQL test URL with CREATE/DROP DATABASE privileges")?;
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
            _ => anyhow::bail!("fixture requires PostgreSQL or MySQL"),
        };
        ensure!(
            !target.path().trim_matches('/').is_empty(),
            "explicit admin database required"
        );
        let name = format!("bitrouter_eval_capture_{}", uuid::Uuid::new_v4().simple());
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
        // Only a confirmed CREATE grants cleanup authority; never adopt an
        // existing database or use FORCE/KILL against another connection.
        let sql = match backend {
            DatabaseBackend::Postgres => format!("CREATE DATABASE \"{name}\" TEMPLATE template0"),
            DatabaseBackend::MySql => {
                format!("CREATE DATABASE `{name}` CHARACTER SET utf8mb4 COLLATE utf8mb4_bin")
            }
            _ => anyhow::bail!("unsupported fixture backend"),
        };
        let created =
            tokio::time::timeout(CLEANUP, admin.execute(Statement::from_string(backend, sql)))
                .await
                .context("fixture CREATE deadline exceeded")
                .and_then(|result| result.map(|_| ()).map_err(anyhow::Error::from));
        if let Err(error) = created {
            let closed = close(&admin).await;
            return combine(
                Err(error.context(format!("CREATE for {name} unconfirmed; no DROP attempted"))),
                vec![closed],
            );
        }
        Ok(Self {
            admin,
            backend,
            name,
            url: target.into(),
            pools: Vec::new(),
            services: Vec::new(),
        })
    }

    pub(in crate::tenancy::tests::execution::native::capture) fn retain(
        &mut self,
        service: &Service,
    ) {
        self.services.push(service.clone());
    }

    pub(in crate::tenancy::tests::execution::native::capture) async fn connect(
        &mut self,
    ) -> Result<DatabaseConnection> {
        let db = connect(&self.url).await?;
        self.pools.push(db.clone());
        let sql = match self.backend {
            DatabaseBackend::Postgres => "SELECT current_database() AS name",
            DatabaseBackend::MySql => "SELECT DATABASE() AS name",
            _ => anyhow::bail!("unsupported fixture backend"),
        };
        let actual = db
            .query_one(Statement::from_string(self.backend, sql.to_owned()))
            .await?
            .context("fixture database identity missing")?;
        ensure!(
            actual.try_get::<String>("", "name")? == self.name,
            "fixture connection escaped its owned database"
        );
        // Each pool has one connection, so these session bounds apply to every
        // statement in that pool, including the real actor repeatable-read operations.
        let settings: &[&str] = match self.backend {
            DatabaseBackend::Postgres => {
                &["SET statement_timeout = '10s'", "SET lock_timeout = '5s'"]
            }
            DatabaseBackend::MySql => &[
                "SET SESSION innodb_lock_wait_timeout = 5",
                "SET SESSION lock_wait_timeout = 5",
                "SET SESSION max_execution_time = 10000",
            ],
            _ => anyhow::bail!("unsupported fixture backend"),
        };
        for sql in settings {
            db.execute_unprepared(sql).await?;
        }
        Ok(db)
    }

    // This is test teardown after all campaign futures/registrations are dropped.
    // It deliberately does not claim a failed observation had a clean shutdown.
    pub(in crate::tenancy::tests::execution::native::capture) async fn quiesce(
        &self,
    ) -> Result<()> {
        let mut outcomes = Vec::new();
        for service in &self.services {
            outcomes.push(
                tokio::time::timeout(CLEANUP, service.shutdown())
                    .await
                    .context("capture fixture host/actor drain did not finish; database retained")
                    .and_then(|result| result)
                    .and_then(|()| service.cleanup.ensure_clean())
                    .and_then(|()| {
                        ensure!(
                            service.host_slots_for_test() == 1,
                            "capture host slot not returned"
                        );
                        Ok(())
                    }),
            );
        }
        combine(Ok(()), outcomes)
    }

    pub(in crate::tenancy::tests::execution::native::capture) async fn close_pools(
        &self,
    ) -> Result<()> {
        self.quiesce().await?;
        let mut outcomes = Vec::new();
        for pool in &self.pools {
            outcomes.push(close(pool).await);
        }
        combine(Ok(()), outcomes)
    }

    /// Unknown consumer cleanup forbids physical DROP even if pool closure
    /// could succeed. No canceled COMMIT/rollback is manufactured in this test.
    pub(in crate::tenancy::tests::execution::native::capture) async fn finish(
        self,
        outcome: Result<()>,
        consumers_clean: bool,
    ) -> Result<()> {
        let mut cleanup = Vec::new();
        let joined = self.quiesce().await;
        let mut closed = consumers_clean && joined.is_ok();
        cleanup.push(joined);
        if !closed {
            cleanup.push(Err(anyhow::anyhow!(
                "fixture {} retained because actor/transaction cleanup is unconfirmed",
                self.name
            )));
            cleanup.push(close(&self.admin).await);
            return combine(outcome, cleanup);
        }
        for pool in &self.pools {
            let result = close(pool).await;
            closed &= result.is_ok();
            cleanup.push(result);
        }
        if closed {
            let sql = match self.backend {
                DatabaseBackend::Postgres => format!("DROP DATABASE \"{}\"", self.name),
                DatabaseBackend::MySql => format!("DROP DATABASE `{}`", self.name),
                _ => String::new(),
            };
            cleanup.push(
                tokio::time::timeout(
                    CLEANUP,
                    self.admin
                        .execute(Statement::from_string(self.backend, sql)),
                )
                .await
                .context("fixture DROP deadline exceeded")
                .and_then(|result| result.map(|_| ()).map_err(anyhow::Error::from))
                .with_context(|| format!("dropping owned fixture database {}", self.name)),
            );
        } else {
            cleanup.push(Err(anyhow::anyhow!(
                "fixture {} retained because a pool did not close",
                self.name
            )));
        }
        cleanup.push(close(&self.admin).await);
        combine(outcome, cleanup)
    }
}

async fn connect(url: &str) -> Result<DatabaseConnection> {
    let mut options = ConnectOptions::new(url);
    options
        .min_connections(1)
        .max_connections(1)
        .connect_timeout(Duration::from_secs(5))
        .acquire_timeout(Duration::from_secs(5))
        .sqlx_logging(false);
    Ok(tokio::time::timeout(CLEANUP, Database::connect(options))
        .await
        .context("fixture connection deadline exceeded")??)
}

pub(in crate::tenancy::tests::execution::native::capture) async fn close(
    db: &DatabaseConnection,
) -> Result<()> {
    tokio::time::timeout(CLEANUP, async {
        loop {
            db.close_by_ref().await?;
            if pool_size(db)? == 0 {
                return Ok::<_, anyhow::Error>(());
            }
            // SQLx may finish a return-to-pool task during close. This drains
            // closure under one deadline; no SQL operation or error is retried.
            // https://github.com/launchbadge/sqlx/blob/v0.8.6/sqlx-core/src/pool/inner.rs
            tokio::task::yield_now().await;
        }
    })
    .await
    .context("fixture pool close deadline exceeded")?
}

fn pool_size(db: &DatabaseConnection) -> Result<u32> {
    match db {
        #[cfg(feature = "db-postgres")]
        DatabaseConnection::SqlxPostgresPoolConnection(_) => {
            Ok(db.get_postgres_connection_pool().size())
        }
        #[cfg(feature = "db-mysql")]
        DatabaseConnection::SqlxMySqlPoolConnection(_) => Ok(db.get_mysql_connection_pool().size()),
        _ => Err(anyhow::anyhow!("fixture pool backend unavailable")),
    }
}

fn combine<T>(primary: Result<T>, cleanup: Vec<Result<()>>) -> Result<T> {
    let errors = cleanup
        .into_iter()
        .filter_map(Result::err)
        .map(|error| format!("{error:#}"))
        .collect::<Vec<_>>();
    if errors.is_empty() {
        return primary;
    }
    let message = format!("fixture cleanup failed: {}", errors.join("; "));
    match primary {
        Err(error) => Err(error.context(message)),
        Ok(_) => Err(anyhow::anyhow!(message)),
    }
}
