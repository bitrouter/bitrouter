//! Own the physical database, pools, and unconsumed transactions outside test timeouts.
use super::*;
use sea_orm::{ConnectOptions, Database, DatabaseTransaction};

const CLEANUP: Duration = Duration::from_secs(10);

pub(super) struct Fixture {
    admin: DatabaseConnection,
    backend: DatabaseBackend,
    name: String,
    url: String,
    pools: Vec<DatabaseConnection>,
    transactions: Vec<Option<DatabaseTransaction>>,
}

impl Fixture {
    pub(super) async fn create() -> Result<Self> {
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
        let name = format!("bitrouter_eval_{}", uuid::Uuid::new_v4().simple());
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
            transactions: Vec::new(),
        })
    }

    pub(super) fn backend(&self) -> DatabaseBackend {
        self.backend
    }

    pub(super) async fn connect(&mut self) -> Result<DatabaseConnection> {
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
        // statement in that pool, including lock waits in the deliberate RR race.
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

    pub(super) async fn start(&mut self, db: &DatabaseConnection) -> Result<usize> {
        let tx = db
            .begin_with_config(Some(IsolationLevel::RepeatableRead), None)
            .await?;
        self.transactions.push(Some(tx));
        Ok(self.transactions.len() - 1)
    }

    pub(super) fn tx(&self, slot: usize) -> Result<&DatabaseTransaction> {
        self.transactions
            .get(slot)
            .and_then(Option::as_ref)
            .context("fixture transaction missing")
    }

    fn take(&mut self, slot: usize) -> Result<DatabaseTransaction> {
        self.transactions
            .get_mut(slot)
            .and_then(Option::take)
            .context("fixture transaction already consumed")
    }

    pub(super) async fn commit(&mut self, slot: usize) -> Result<()> {
        self.take(slot)?.commit().await?;
        Ok(())
    }

    pub(super) async fn rollback(&mut self, slot: usize) -> Result<()> {
        self.take(slot)?.rollback().await?;
        Ok(())
    }

    /// Called after the campaign future is dropped, on success, Err, or timeout.
    /// No test task survives this call. All borrowed, unconsumed transactions
    /// receive explicit rollback before pool closure and physical database DROP.
    pub(super) async fn finish(mut self, outcome: Result<()>) -> Result<()> {
        let mut cleanup = Vec::new();
        for tx in &mut self.transactions {
            if let Some(tx) = tx.take() {
                cleanup.push(
                    tokio::time::timeout(CLEANUP, tx.rollback())
                        .await
                        .context("fixture rollback deadline exceeded")
                        .and_then(|result| result.map_err(anyhow::Error::from)),
                );
            }
        }
        let mut closed = true;
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

pub(super) async fn close(db: &DatabaseConnection) -> Result<()> {
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
