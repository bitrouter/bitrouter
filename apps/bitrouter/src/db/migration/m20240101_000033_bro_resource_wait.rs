//! Prevent an older Goal writer from discarding unfamiliar resource-wait facts.
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
            "BRO resource waits require a compatible writer; their format fence cannot be removed"
                .into(),
        ))
    }
}
