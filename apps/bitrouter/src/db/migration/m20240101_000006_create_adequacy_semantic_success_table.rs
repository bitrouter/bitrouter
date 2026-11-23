//! Persist task-level success evidence for learned cheap-route locks.

use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum AdequacySemanticSuccess {
    Table,
    EvidenceId,
    Fingerprint,
    TaskId,
    CreatedAt,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    manager
        .create_table(
            Table::create()
                .table(AdequacySemanticSuccess::Table)
                .if_not_exists()
                .col(
                    ColumnDef::new(AdequacySemanticSuccess::EvidenceId)
                        .string()
                        .not_null()
                        .primary_key(),
                )
                .col(
                    ColumnDef::new(AdequacySemanticSuccess::Fingerprint)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(AdequacySemanticSuccess::TaskId)
                        .string()
                        .not_null(),
                )
                .col(
                    ColumnDef::new(AdequacySemanticSuccess::CreatedAt)
                        .string()
                        .not_null(),
                )
                .to_owned(),
        )
        .await?;
    manager
        .create_index(
            Index::create()
                .if_not_exists()
                .name("idx_adequacy_semantic_success_fingerprint")
                .table(AdequacySemanticSuccess::Table)
                .col(AdequacySemanticSuccess::Fingerprint)
                .to_owned(),
        )
        .await?;
    Ok(())
}
