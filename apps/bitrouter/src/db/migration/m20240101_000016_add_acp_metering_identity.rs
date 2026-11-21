//! Add nullable ACP/native request-session correlation to OSS metering.

use sea_orm_migration::prelude::*;

#[derive(DeriveIden)]
enum Requests {
    Table,
    AgentHarness,
    ControllerInstanceId,
    AcpSessionId,
    NativeRootSessionId,
    NativeAgentThreadId,
    NativeParentAgentThreadId,
    NativeTurnId,
    RouteLeaseId,
    SessionIdentityJson,
}

/// DDL-only component of the fresh current baseline; never accepts an old dataset.
pub(super) async fn initialize_current(manager: &SchemaManager<'_>) -> Result<(), DbErr> {
    for column in [
        ColumnDef::new(Requests::AgentHarness).string().to_owned(),
        ColumnDef::new(Requests::ControllerInstanceId)
            .string()
            .to_owned(),
        ColumnDef::new(Requests::AcpSessionId).string().to_owned(),
        ColumnDef::new(Requests::NativeRootSessionId)
            .string()
            .to_owned(),
        ColumnDef::new(Requests::NativeAgentThreadId)
            .string()
            .to_owned(),
        ColumnDef::new(Requests::NativeParentAgentThreadId)
            .string()
            .to_owned(),
        ColumnDef::new(Requests::NativeTurnId).string().to_owned(),
        ColumnDef::new(Requests::RouteLeaseId).string().to_owned(),
        ColumnDef::new(Requests::SessionIdentityJson)
            .text()
            .to_owned(),
    ] {
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(column)
                    .to_owned(),
            )
            .await?;
    }
    Ok(())
}
