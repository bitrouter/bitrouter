//! An isolated catalog for destructive fixture DDL, owned from CREATE to DROP.
use anyhow::{Context, Result, ensure};
use sea_orm::{ConnectOptions, ConnectionTrait, Database, DatabaseBackend, DatabaseConnection};

pub(crate) struct OwnedServer {
    admin: DatabaseConnection,
    pub(crate) database: DatabaseConnection,
    name: String,
    pub(crate) url: String,
}

impl OwnedServer {
    pub(crate) async fn new(backend: DatabaseBackend, url_env: &str) -> Result<Self> {
        let address = std::env::var(url_env)
            .context("dedicated migration database URL required; no fallback")?;
        let mut selected = url::Url::parse(&address).context("invalid dedicated database URL")?;
        ensure!(
            match backend {
                DatabaseBackend::Postgres => matches!(selected.scheme(), "postgres" | "postgresql"),
                DatabaseBackend::MySql => selected.scheme() == "mysql",
                _ => false,
            },
            "wrong backend selected for schema fixture"
        );
        ensure!(
            !selected.path().trim_matches('/').is_empty(),
            "{url_env} must explicitly name a dedicated test database"
        );
        let mut options = ConnectOptions::new(address);
        options
            .max_connections(1)
            .min_connections(1)
            .sqlx_logging(false);
        let admin = Database::connect(options)
            .await
            .context("baseline admin connection unavailable")?;
        let name = format!("bitrouter_schema_{}", uuid::Uuid::new_v4().simple());
        let create = match backend {
            DatabaseBackend::Postgres => format!("CREATE DATABASE {name}"),
            DatabaseBackend::MySql => {
                format!("CREATE DATABASE {name} CHARACTER SET utf8mb4 COLLATE utf8mb4_0900_ai_ci")
            }
            _ => anyhow::bail!("unsupported schema fixture backend"),
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
            url: selected.to_string(),
        })
    }

    pub(crate) async fn finish(self, result: Result<()>) -> Result<()> {
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
