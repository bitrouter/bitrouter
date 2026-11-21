//! Fresh current schema installation and read-only catalog validation.
use sea_orm::{ConnectionTrait, DatabaseBackend, DbErr};
use sea_orm_migration::{MigrationTrait, MigratorTrait};

mod baseline;
#[cfg(test)]
mod catalog_tests;
pub(crate) mod provider_technical;
#[cfg(all(test, any(feature = "db-postgres", feature = "db-mysql")))]
pub(crate) mod server_fixture;
#[cfg(test)]
mod tests;

async fn validate_tables(db: &impl ConnectionTrait, tables: &[&str]) -> Result<(), DbErr> {
    if db.get_database_backend() == DatabaseBackend::Sqlite {
        crate::db::sqlite_schema::validate_tables(db, tables)
            .await
            .map_err(|error| DbErr::Custom(error.to_string()))
    } else {
        crate::db::current_schema::validate(db).await
    }
}

/// Check immutable receipts, usage and their archive without changing stored bytes.
pub(crate) async fn validate_receipt_archive(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    validate_tables(
        db,
        &[
            "tenancy_execution_receipts",
            "tenancy_usage_evidence",
            "tenancy_receipt_archive",
        ],
    )
    .await
}

/// Check OAuth ownership and the pending-work projection before reading authority.
pub(crate) async fn validate_owned_oauth(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    validate_unknown_obligations(db).await
}

/// Check the exact pending-work catalog without deriving new authority facts.
pub(crate) async fn validate_unknown_obligations(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    validate_tables(
        db,
        &[
            "tenancy_unknown_obligations",
            "tenancy_oauth_slots",
            "tenancy_receipt_auxiliary",
        ],
    )
    .await
}

/// Check the immutable evidence catalog and its receipt/archive linkage.
pub(crate) async fn validate_usage_evidence(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    validate_receipt_archive(db).await
}

/// Check current authority objects without changing ownership or configuration.
pub(crate) async fn validate_authority(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    validate_tables(
        db,
        &[
            "tenancy_installation",
            "tenancy_tenants",
            "tenancy_principals",
            "tenancy_bindings",
            "tenancy_credentials",
            "tenancy_operations",
            "tenancy_configurations",
            "tenancy_active_configurations",
            "tenancy_resource_revisions",
            "tenancy_resource_lifecycle",
        ],
    )
    .await
}

/// Check dispatch intent and immutable receipt metadata before interpreting receipts.
pub(crate) async fn validate_ordered_receipts(db: &impl ConnectionTrait) -> Result<(), DbErr> {
    validate_tables(
        db,
        &[
            "tenancy_dispatch_intents",
            "tenancy_execution_receipts",
            "tenancy_usage_evidence",
            "tenancy_receipt_archive",
        ],
    )
    .await
}

/// Installs the current schema only on an empty dataset, otherwise validates it.
pub struct Migrator;

#[async_trait::async_trait]
impl MigratorTrait for Migrator {
    async fn up<'c, C>(db: C, _steps: Option<u32>) -> Result<(), DbErr>
    where
        C: sea_orm_migration::IntoSchemaManagerConnection<'c>,
    {
        baseline::open(db.into_schema_manager_connection()).await
    }
    fn migrations() -> Vec<Box<dyn MigrationTrait>> {
        vec![Box::new(baseline::Migration)]
    }
}
