//! Historical component fixtures retain the production backend transaction/fence.
use super::{DatabaseConnection, migration::Migrator};
use sea_orm_migration::{MigrationName, MigrationTrait, MigratorTrait};

#[cfg(feature = "db-mysql")]
pub(crate) mod mysql_race;

struct ThroughSharedHealth;

#[async_trait::async_trait]
impl MigratorTrait for ThroughSharedHealth {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        let successor = super::migration::m20240101_000048_native_generate_content::Migration;
        Migrator::migrations()
            .into_iter()
            .take_while(|migration| migration.name() != successor.name())
            .collect()
    }
}

pub(crate) async fn through_shared_health(db: &DatabaseConnection) -> super::Result<()> {
    super::run_migrations_with::<ThroughSharedHealth>(db).await
}
