//! Compatibility fence for Goal snapshots that reference managed execution.
//!
//! SeaORM persists this migration name even though no table changes are needed.
//! Earlier binaries reject an applied migration absent from their own list,
//! before starting a Goal coordinator that could erase unfamiliar associations.
use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }

    async fn down(&self, _: &SchemaManager) -> Result<(), DbErr> {
        Err(DbErr::Custom(
            "BRO managed Goal execution history requires a compatible writer; removing its version fence is unsafe".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_orchestrator::goal::{GoalCommand, GoalReply, GoalService, GoalStore};
    use sea_orm_migration::{MigrationTrait, MigratorTrait, SchemaManager};

    use super::Migration;
    use crate::db::migration::Migrator;

    // The pre-refactor binary's migration list, using the same pinned SeaORM
    // resolver. This tests startup compatibility, not an arbitrary raw SQL writer.
    struct LegacyMigrator;
    #[async_trait::async_trait]
    impl MigratorTrait for LegacyMigrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            Migrator::migrations()
                .into_iter()
                .take_while(|migration| {
                    migration.name() != "m20240101_000030_bro_goal_execution_format"
                })
                .collect()
        }
    }

    struct BeforeWorkerSourcesMigrator;
    #[async_trait::async_trait]
    impl MigratorTrait for BeforeWorkerSourcesMigrator {
        fn migrations() -> Vec<Box<dyn MigrationTrait>> {
            Migrator::migrations()
                .into_iter()
                .take_while(|migration| {
                    migration.name() != "m20240101_000031_bro_worker_source_format"
                })
                .collect()
        }
    }

    #[tokio::test]
    async fn legacy_startup_rejects_managed_goal_format_without_rewriting_old_goals()
    -> anyhow::Result<()> {
        let directory = tempfile::tempdir()?;
        let url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("state.db").display()
        );
        let db = crate::db::connect(&url).await?;
        LegacyMigrator::up(&db, None).await?;
        let store = Arc::new(crate::goal_store::DatabaseGoalStore::new(db.clone()));
        let goals = GoalService::new(store.clone());
        let GoalReply::Accepted { operation } = goals
            .execute(
                "local",
                GoalCommand::Create {
                    operation_id: "legacy-format-goal".into(),
                    title: "Retain old Goal history".into(),
                    workspace: directory.path().into(),
                    objective: "Read this Goal without starting execution".into(),
                    budget_limits: Default::default(),
                },
                1,
            )
            .await?
        else {
            anyhow::bail!("Goal creation missing")
        };
        let original = store.get("local", &operation.goal_id).await?;
        Migrator::up(&db, None).await?;
        assert_eq!(store.get("local", &operation.goal_id).await?, original);
        let failure = LegacyMigrator::up(&db, None).await.err().ok_or_else(|| {
            anyhow::anyhow!("old app accepted a database containing new Goal execution identities")
        })?;
        assert!(
            failure
                .to_string()
                .contains("000030_bro_goal_execution_format")
        );
        assert!(Migration.down(&SchemaManager::new(&db)).await.is_err());
        assert!(
            BeforeWorkerSourcesMigrator::up(&db, None)
                .await
                .err()
                .is_some_and(|error| error
                    .to_string()
                    .contains("000031_bro_worker_source_format"))
        );
        assert!(
            crate::db::migration::m20240101_000031_bro_worker_source_format::Migration
                .down(&SchemaManager::new(&db))
                .await
                .is_err()
        );
        drop(goals);
        drop(store);
        db.close().await?;
        let reopened = crate::db::connect(&url).await?;
        crate::db::run_migrations(&reopened).await?;
        let store = crate::goal_store::DatabaseGoalStore::new(reopened.clone());
        assert_eq!(store.get("local", &operation.goal_id).await?, original);
        let history = store
            .history("local", &operation.goal_id, 0, None, 16)
            .await?;
        assert_eq!(history.operations, vec![operation]);
        assert!(LegacyMigrator::up(&reopened, None).await.is_err());
        assert!(
            BeforeWorkerSourcesMigrator::up(&reopened, None)
                .await
                .is_err()
        );
        reopened.close().await?;
        Ok(())
    }
}
