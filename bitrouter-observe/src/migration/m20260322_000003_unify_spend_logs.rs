//! Unifies the spend_logs table for model, tool, and agent service types.
//!
//! - Adds `service_type` column (default `"model"` for existing rows).
//! - Renames `model` → `operation`, `provider` → `service_name`,
//!   `error_type` → `error_info`.
//!
//! SQLite does not support `ALTER TABLE … RENAME COLUMN` reliably across
//! all versions, so this migration recreates the table with the new schema,
//! copies data, and swaps.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // 1. Create the new table with the unified schema.
        manager
            .create_table(
                Table::create()
                    .table(SpendLogsNew::Table)
                    .col(
                        ColumnDef::new(SpendLogsNew::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(
                        ColumnDef::new(SpendLogsNew::ServiceType)
                            .string()
                            .not_null()
                            .default("model"),
                    )
                    .col(ColumnDef::new(SpendLogsNew::AccountId).string().null())
                    .col(ColumnDef::new(SpendLogsNew::SessionId).uuid().null())
                    .col(
                        ColumnDef::new(SpendLogsNew::ServiceName)
                            .string()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SpendLogsNew::Operation).string().not_null())
                    .col(
                        ColumnDef::new(SpendLogsNew::InputTokens)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(SpendLogsNew::OutputTokens)
                            .integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(SpendLogsNew::Cost)
                            .double()
                            .not_null()
                            .default(0.0),
                    )
                    .col(
                        ColumnDef::new(SpendLogsNew::LatencyMs)
                            .big_integer()
                            .not_null()
                            .default(0),
                    )
                    .col(
                        ColumnDef::new(SpendLogsNew::Success)
                            .boolean()
                            .not_null()
                            .default(true),
                    )
                    .col(ColumnDef::new(SpendLogsNew::ErrorInfo).string().null())
                    .col(
                        ColumnDef::new(SpendLogsNew::CreatedAt)
                            .timestamp()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        // 2. Copy data from old table, mapping columns.
        //    operation = provider || ':' || model
        //    service_name = provider  (route name not available in old data)
        //    error_info = error_type
        let db = manager.get_connection();
        db.execute_unprepared(
            "INSERT INTO spend_logs_new (id, service_type, account_id, session_id, service_name, operation, input_tokens, output_tokens, cost, latency_ms, success, error_info, created_at)
             SELECT id, 'model', account_id, session_id, provider, provider || ':' || model, input_tokens, output_tokens, cost, latency_ms, success, error_type, created_at
             FROM spend_logs",
        )
        .await?;

        // 3. Drop old table.
        manager
            .drop_table(Table::drop().table(SpendLogsOld::Table).to_owned())
            .await?;

        // 4. Rename new table to spend_logs.
        manager
            .rename_table(
                Table::rename()
                    .table(SpendLogsNew::Table, SpendLogsOld::Table)
                    .to_owned(),
            )
            .await?;

        // 5. Recreate indexes on the renamed table.
        manager
            .create_index(
                Index::create()
                    .name("idx_spend_logs_account_created")
                    .table(SpendLogsOld::Table)
                    .col(SpendLogsNew::AccountId)
                    .col(SpendLogsNew::CreatedAt)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_spend_logs_session_id")
                    .table(SpendLogsOld::Table)
                    .col(SpendLogsNew::SessionId)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_spend_logs_service_type_account_created")
                    .table(SpendLogsOld::Table)
                    .col(SpendLogsNew::ServiceType)
                    .col(SpendLogsNew::AccountId)
                    .col(SpendLogsNew::CreatedAt)
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        // Reverse: recreate original schema and copy data back.
        manager
            .create_table(
                Table::create()
                    .table(SpendLogsNew::Table)
                    .col(
                        ColumnDef::new(SpendLogsNew::Id)
                            .uuid()
                            .not_null()
                            .primary_key(),
                    )
                    .col(ColumnDef::new(SpendLogsOldCols::AccountId).string().null())
                    .col(ColumnDef::new(SpendLogsOldCols::SessionId).uuid().null())
                    .col(ColumnDef::new(SpendLogsOldCols::Model).string().not_null())
                    .col(
                        ColumnDef::new(SpendLogsOldCols::Provider)
                            .string()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SpendLogsOldCols::InputTokens)
                            .integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SpendLogsOldCols::OutputTokens)
                            .integer()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SpendLogsOldCols::Cost).double().not_null())
                    .col(
                        ColumnDef::new(SpendLogsOldCols::LatencyMs)
                            .big_integer()
                            .not_null(),
                    )
                    .col(
                        ColumnDef::new(SpendLogsOldCols::Success)
                            .boolean()
                            .not_null(),
                    )
                    .col(ColumnDef::new(SpendLogsOldCols::ErrorType).string().null())
                    .col(
                        ColumnDef::new(SpendLogsOldCols::CreatedAt)
                            .timestamp()
                            .not_null(),
                    )
                    .to_owned(),
            )
            .await?;

        // Copy model rows back, splitting operation back into provider + model.
        // Only model rows can be restored; tool/agent rows are dropped.
        let db = manager.get_connection();
        db.execute_unprepared(
            "INSERT INTO spend_logs_new (id, account_id, session_id, model, provider, input_tokens, output_tokens, cost, latency_ms, success, error_type, created_at)
             SELECT id, account_id, session_id, operation, service_name, input_tokens, output_tokens, cost, latency_ms, success, error_info, created_at
             FROM spend_logs
             WHERE service_type = 'model'",
        )
        .await?;

        manager
            .drop_table(Table::drop().table(SpendLogsOld::Table).to_owned())
            .await?;

        manager
            .rename_table(
                Table::rename()
                    .table(SpendLogsNew::Table, SpendLogsOld::Table)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_spend_logs_account_created")
                    .table(SpendLogsOld::Table)
                    .col(SpendLogsOldCols::AccountId)
                    .col(SpendLogsOldCols::CreatedAt)
                    .to_owned(),
            )
            .await?;

        manager
            .create_index(
                Index::create()
                    .name("idx_spend_logs_session_id")
                    .table(SpendLogsOld::Table)
                    .col(SpendLogsOldCols::SessionId)
                    .to_owned(),
            )
            .await
    }
}

/// New unified table identifiers (also used as the final table after rename).
#[derive(DeriveIden)]
enum SpendLogsNew {
    #[sea_orm(iden = "spend_logs_new")]
    Table,
    Id,
    ServiceType,
    AccountId,
    SessionId,
    ServiceName,
    Operation,
    InputTokens,
    OutputTokens,
    Cost,
    LatencyMs,
    Success,
    ErrorInfo,
    CreatedAt,
}

/// Old table identifiers (for drop/rename).
#[derive(DeriveIden)]
enum SpendLogsOld {
    #[sea_orm(iden = "spend_logs")]
    Table,
}

/// Old column identifiers (for the down migration).
#[derive(DeriveIden)]
enum SpendLogsOldCols {
    AccountId,
    SessionId,
    Model,
    Provider,
    InputTokens,
    OutputTokens,
    Cost,
    LatencyMs,
    Success,
    ErrorType,
    CreatedAt,
}
