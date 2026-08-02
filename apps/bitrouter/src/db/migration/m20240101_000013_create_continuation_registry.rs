//! Create the encrypted, owner-scoped provider continuation registry.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager.create_table(provider_continuations_table()).await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_provider_continuations_owner")
                    .if_not_exists()
                    .table(ProviderContinuations::Table)
                    .col(ProviderContinuations::OwnerIdentity)
                    .col(ProviderContinuations::ContinuationIdentity)
                    .unique()
                    .to_owned(),
            )
            .await?;
        manager
            .create_index(
                Index::create()
                    .name("idx_provider_continuations_purge_after")
                    .if_not_exists()
                    .table(ProviderContinuations::Table)
                    .col(ProviderContinuations::PurgeAfter)
                    .to_owned(),
            )
            .await?;

        manager
            .create_table(
                Table::create()
                    .table(ProviderContinuationKeyEpoch::Table)
                    .if_not_exists()
                    .col(
                        ColumnDef::new(ProviderContinuationKeyEpoch::SingletonId)
                            .integer()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(ProviderContinuationKeyEpoch::KeyId)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(ProviderContinuationKeyEpoch::CreatedAt)
                            .string()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .drop_table(
                Table::drop()
                    .table(ProviderContinuationKeyEpoch::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await?;
        manager
            .drop_table(
                Table::drop()
                    .table(ProviderContinuations::Table)
                    .if_exists()
                    .to_owned(),
            )
            .await
    }
}

pub(crate) fn provider_continuations_table() -> TableCreateStatement {
    Table::create()
        .table(ProviderContinuations::Table)
        .if_not_exists()
        .col(
            ColumnDef::new(ProviderContinuations::ContinuationIdentity)
                .string()
                .not_null()
                .primary_key(),
        )
        .col(
            ColumnDef::new(ProviderContinuations::OwnerIdentity)
                .string()
                .not_null(),
        )
        .col(ColumnDef::new(ProviderContinuations::Ciphertext).text())
        .col(ColumnDef::new(ProviderContinuations::Nonce).string())
        .col(
            ColumnDef::new(ProviderContinuations::TargetFingerprint)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(ProviderContinuations::KeyId)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(ProviderContinuations::CipherVersion)
                .integer()
                .not_null(),
        )
        .col(
            ColumnDef::new(ProviderContinuations::CreatedAt)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(ProviderContinuations::ExpiresAt)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(ProviderContinuations::PurgeAfter)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(ProviderContinuations::PublicationState)
                .string()
                .not_null(),
        )
        .col(
            ColumnDef::new(ProviderContinuations::PublicationGeneration)
                .string()
                .not_null(),
        )
        .check(Expr::col(ProviderContinuations::PublicationState).is_in(["provisional", "active"]))
        .check(Expr::col(ProviderContinuations::PublicationGeneration).ne(""))
        .to_owned()
}

#[derive(DeriveIden)]
enum ProviderContinuations {
    Table,
    OwnerIdentity,
    ContinuationIdentity,
    Ciphertext,
    Nonce,
    TargetFingerprint,
    KeyId,
    CipherVersion,
    CreatedAt,
    ExpiresAt,
    PurgeAfter,
    PublicationState,
    PublicationGeneration,
}

#[derive(DeriveIden)]
enum ProviderContinuationKeyEpoch {
    Table,
    SingletonId,
    KeyId,
    CreatedAt,
}
