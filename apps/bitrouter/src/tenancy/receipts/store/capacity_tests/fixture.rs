use super::*;

pub(super) struct Fixture {
    pub db: DatabaseConnection,
    owned: Option<crate::db::OwnedDatabase>,
    url: String,
    #[cfg(feature = "db-postgres")]
    server: Option<crate::db::migration::server_fixture::OwnedServer>,
    _directory: Option<tempfile::TempDir>,
}

impl Fixture {
    pub async fn new(backend: DatabaseBackend, legacy: bool) -> Result<Self> {
        #[cfg(feature = "db-postgres")]
        let mut server = None;
        let mut directory = None;
        let url = match backend {
            DatabaseBackend::Sqlite => {
                if cfg!(unix) {
                    let home = tempfile::tempdir()?;
                    let url = format!(
                        "sqlite://{}?mode=rwc",
                        home.path().join("capacity.sqlite").display()
                    );
                    directory = Some(home);
                    url
                } else {
                    "sqlite::memory:".into()
                }
            }
            #[cfg(feature = "db-postgres")]
            DatabaseBackend::Postgres => {
                let isolated = crate::db::migration::server_fixture::OwnedServer::new(
                    backend,
                    "BITROUTER_TEST_MIGRATION_DATABASE_URL",
                )
                .await?;
                // Work uses the actual one-session owned connector, not the
                // fixture's ordinary catalog administration connection.
                isolated.database.clone().close().await?;
                let url = isolated.url.clone();
                server = Some(isolated);
                url
            }
            _ => anyhow::bail!("capacity contract backend unavailable"),
        };
        let mut fixture = Self {
            db: DatabaseConnection::Disconnected,
            owned: None,
            url,
            #[cfg(feature = "db-postgres")]
            server,
            _directory: directory,
        };
        let initialized: Result<()> = async {
            if fixture.url == "sqlite::memory:" {
                fixture.db = crate::db::connect(&fixture.url).await?;
            } else {
                let owned = crate::db::connect_owned_tenancy(&fixture.url).await?;
                fixture.db = owned.database().clone();
                fixture.owned = Some(owned);
            }
            if legacy {
                // Start with the old renderer and old marker, before receipts exist.
                // Never relabel a fresh current native PostgreSQL catalog.
                for ddl in crate::db::current_schema::legacy_statements(backend)? {
                    fixture.db.execute_unprepared(&ddl).await?;
                }
                let ledger = Query::insert()
                    .into_table(Alias::new("seaql_migrations"))
                    .columns(["version", "applied_at"].map(Alias::new))
                    .values([
                        store_format::legacy_marker(backend).into(),
                        chrono::Utc::now().timestamp().into(),
                    ])?
                    .to_owned();
                fixture.db.execute(backend.build(&ledger)).await?;
                ensure!(store_format::inspect(&fixture.db).await? == store_format::State::Legacy);
            } else {
                fixture.migrate().await?;
            }
            Self::header(&fixture.db).await?;
            Ok(())
        }
        .await;
        if let Err(error) = initialized {
            return match fixture.finish(Err(error)).await {
                Err(error) => Err(error),
                Ok(()) => anyhow::bail!("failed capacity fixture initialization returned success"),
            };
        }
        Ok(fixture)
    }

    pub async fn header(db: &impl ConnectionTrait) -> Result<()> {
        let header = Query::insert()
            .into_table(Alias::new("tenancy_installation"))
            .columns(
                [
                    "singleton",
                    "installation_id",
                    "schema_version",
                    "authority_revision",
                    "operation_ids",
                    "safety_operation_ids",
                    "saturated",
                    "created_at",
                ]
                .map(Alias::new),
            )
            .values([
                1_i32.into(),
                "installation".into(),
                crate::tenancy::AUTHORITY_SCHEMA_VERSION.into(),
                0_i64.into(),
                0_i64.into(),
                0_i64.into(),
                false.into(),
                "2026-10-25T00:00:00Z".into(),
            ])?
            .to_owned();
        db.execute(db.get_database_backend().build(&header)).await?;
        Ok(())
    }

    pub async fn migrate(&self) -> Result<()> {
        match &self.owned {
            Some(owned) => owned.migrate().await,
            None => Ok(crate::db::run_migrations(&self.db).await?),
        }
    }

    pub async fn reopen(&mut self) -> Result<()> {
        if self.owned.is_some() {
            // The pool retains the SQLite file lease even after close. Drop
            // every old wrapper before acquiring the next exclusive owner.
            let db = std::mem::replace(&mut self.db, DatabaseConnection::Disconnected);
            let closed = db.close().await;
            drop(self.owned.take());
            closed?;
            let owned = crate::db::connect_owned_tenancy(&self.url).await?;
            self.db = owned.database().clone();
            self.owned = Some(owned);
        }
        self.migrate().await
    }

    pub async fn legacy_seed(&self) -> Result<sea_orm::DatabaseTransaction> {
        let tx = self.db.begin().await?;
        // Fixture-only scaffold lets current canonical writers encode the old
        // sources without copying product logic. Removed before seed COMMIT.
        capacity::ensure_table(&sea_orm_migration::SchemaManager::new(&tx)).await?;
        capacity::initialize(&tx, &Inventory::default(), false).await?;
        Ok(tx)
    }

    pub async fn finish_legacy_seed(&self, tx: sea_orm::DatabaseTransaction) -> Result<()> {
        tx.execute_unprepared("DROP TABLE tenancy_receipt_capacity")
            .await?;
        tx.commit().await?;
        ensure!(
            !sea_orm_migration::SchemaManager::new(&self.db)
                .has_table(capacity::TABLE)
                .await?
        );
        ensure!(store_format::inspect(&self.db).await? == store_format::State::Legacy);
        if self.db.get_database_backend() == DatabaseBackend::Postgres {
            crate::db::current_schema::validate_legacy(&self.db).await?;
        }
        Ok(())
    }

    pub async fn finish(mut self, outcome: Result<()>) -> Result<()> {
        let closed = self.db.close().await;
        drop(self.owned.take());
        let outcome = outcome.and(closed.map_err(Into::into));
        #[cfg(feature = "db-postgres")]
        if let Some(server) = self.server.take() {
            return server.finish(outcome).await;
        }
        outcome
    }
}
