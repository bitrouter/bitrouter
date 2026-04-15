use sea_orm_migration::{MigrationTrait, MigratorTrait};

/// Assembled migrator for the bitrouter binary.
///
/// Collects migrations from all crates that contribute database schema.
pub struct Migrator;

impl MigratorTrait for Migrator {
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        let mut all = Vec::new();
        all.extend(bitrouter_accounts::migration::migrations());
        all.extend(bitrouter_observe::migration::migrations());
        all
    }
}

/// Run all pending migrations against the given database connection.
pub async fn migrate(db: &sea_orm::DatabaseConnection) -> Result<(), sea_orm::DbErr> {
    tracing::info!("running pending database migrations");
    Migrator::up(db, None).await?;
    tracing::info!("database migrations complete");
    Ok(())
}
