//! Fence application writers that would erase per-attempt worker provenance.
//! This records a format boundary without new tables or history conversion.
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
            "BRO worker source history requires a compatible writer; its format fence cannot be removed".into(),
        ))
    }
}
