//! Rename the pre-OSS-refactor `requests.final_charge_micro_usd` column to
//! the current `estimated_charge_micro_usd`.
//!
//! The pre-OSS-refactor schema named the column `final_charge_micro_usd`.
//! The OSS metering module renamed it to `estimated_charge_micro_usd` for
//! semantic clarity — we *measure*, cloud bills. A database created by the
//! old code carries the legacy column, and an OSS recorder insert against
//! it fails with `no such column: estimated_charge_micro_usd`.
//!
//! On a fresh database the previous migration already creates the column
//! under its current name, so this migration inspects the live schema
//! (portably, via `SchemaManager::has_column`) and renames in place only
//! when the legacy column is the one actually present.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

const LEGACY: &str = "final_charge_micro_usd";
const CURRENT: &str = "estimated_charge_micro_usd";

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let has_legacy = manager.has_column("requests", LEGACY).await?;
        let has_current = manager.has_column("requests", CURRENT).await?;
        if has_legacy && !has_current {
            manager
                .alter_table(
                    Table::alter()
                        .table(Requests::Table)
                        .rename_column(Alias::new(LEGACY), Alias::new(CURRENT))
                        .to_owned(),
                )
                .await?;
            tracing::info!("metering: renamed legacy `requests.{LEGACY}` → `{CURRENT}`");
        }
        Ok(())
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        let has_legacy = manager.has_column("requests", LEGACY).await?;
        let has_current = manager.has_column("requests", CURRENT).await?;
        if has_current && !has_legacy {
            manager
                .alter_table(
                    Table::alter()
                        .table(Requests::Table)
                        .rename_column(Alias::new(CURRENT), Alias::new(LEGACY))
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
}
