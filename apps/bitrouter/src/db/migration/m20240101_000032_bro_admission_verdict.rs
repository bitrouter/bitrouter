//! Fence writers that cannot retain a managed dispatch's no-admission verdict.
//! Existing tables, snapshots and immutable history remain in place.
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
            "BRO admission verdicts require a compatible writer; the format fence cannot be removed".into(),
        ))
    }
}
