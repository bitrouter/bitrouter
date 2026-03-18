//! Formerly created the `api_keys` table. This migration is now a no-op
//! because JWT-based auth (M6) replaced API key auth. The table was dropped
//! in the old M6 migration; both are now cleaned up.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }

    async fn down(&self, _manager: &SchemaManager) -> Result<(), DbErr> {
        Ok(())
    }
}
