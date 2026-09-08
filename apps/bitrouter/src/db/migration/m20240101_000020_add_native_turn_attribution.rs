//! Preserve explicit native turn ancestry on each metered request. Existing
//! rows remain NULL: a session or thread ID cannot reconstruct these claims.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [Requests::NativeParentTurnId, Requests::NativeRootTurnId] {
            // SQLite/MySQL can commit a column before the migrator records
            // completion. Continue that partial migration on the next start.
            if manager.has_column("requests", column.to_string()).await? {
                continue;
            }
            manager
                .alter_table(
                    Table::alter()
                        .table(Requests::Table)
                        .add_column(ColumnDef::new(column).string().to_owned())
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        for column in [Requests::NativeRootTurnId, Requests::NativeParentTurnId] {
            if !manager.has_column("requests", column.to_string()).await? {
                continue;
            }
            manager
                .alter_table(
                    Table::alter()
                        .table(Requests::Table)
                        .drop_column(column)
                        .to_owned(),
                )
                .await?;
        }
        Ok(())
    }
}

#[derive(DeriveIden)]
enum Requests {
    Table,
    NativeParentTurnId,
    NativeRootTurnId,
}

#[cfg(test)]
mod tests {
    use super::*;
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    #[tokio::test]
    async fn interrupted_column_changes_resume_without_migration_registration() -> anyhow::Result<()>
    {
        for columns in [
            vec![Requests::NativeParentTurnId],
            vec![Requests::NativeParentTurnId, Requests::NativeRootTurnId],
        ] {
            let db = crate::db::connect("sqlite::memory:").await?;
            db.execute_unprepared("CREATE TABLE requests (request_id TEXT PRIMARY KEY)")
                .await?;
            let manager = SchemaManager::new(&db);
            for column in columns {
                manager
                    .alter_table(
                        Table::alter()
                            .table(Requests::Table)
                            .add_column(ColumnDef::new(column).string())
                            .to_owned(),
                    )
                    .await?;
            }
            assert!(!manager.has_table("seaql_migrations").await?);
            Migration.up(&manager).await?;
            Migration.up(&manager).await?;
            assert!(
                manager
                    .has_column("requests", "native_parent_turn_id")
                    .await?
            );
            assert!(
                manager
                    .has_column("requests", "native_root_turn_id")
                    .await?
            );
            // Simulate an interrupted down after only the first DROP commits.
            manager
                .alter_table(
                    Table::alter()
                        .table(Requests::Table)
                        .drop_column(Requests::NativeRootTurnId)
                        .to_owned(),
                )
                .await?;
            Migration.down(&manager).await?;
            Migration.down(&manager).await?;
            assert!(
                !manager
                    .has_column("requests", "native_parent_turn_id")
                    .await?
            );
            assert!(
                !manager
                    .has_column("requests", "native_root_turn_id")
                    .await?
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn existing_turns_remain_unattributed_after_upgrade() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        db.execute_unprepared(
            "CREATE TABLE requests (request_id TEXT PRIMARY KEY, native_turn_id TEXT NOT NULL)",
        )
        .await?;
        db.execute_unprepared("INSERT INTO requests VALUES ('old-request', 'old-turn')")
            .await?;
        let manager = SchemaManager::new(&db);
        Migration.up(&manager).await?;
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT native_turn_id, native_parent_turn_id, native_root_turn_id FROM requests WHERE request_id = 'old-request'",
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("old request missing after migration"))?;
        assert_eq!(row.try_get::<String>("", "native_turn_id")?, "old-turn");
        assert_eq!(
            row.try_get::<Option<String>>("", "native_parent_turn_id")?,
            None
        );
        assert_eq!(
            row.try_get::<Option<String>>("", "native_root_turn_id")?,
            None
        );
        Migration.down(&manager).await?;
        assert!(
            !manager
                .has_column("requests", "native_parent_turn_id")
                .await?
        );
        assert!(
            !manager
                .has_column("requests", "native_root_turn_id")
                .await?
        );
        assert!(manager.has_column("requests", "native_turn_id").await?);
        Ok(())
    }
}
