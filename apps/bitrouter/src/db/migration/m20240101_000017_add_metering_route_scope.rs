//! Persist the route namespace a routed ACP request was attributed under.
//!
//! `m20240101_000016` stored the controller a request *declares*, which is a
//! claim nothing verifies. Route leases are namespaced by the API principal so
//! one caller's cannot be reached through another's; the spend query behind
//! session-attributed cost had no such column to filter on, so two principals
//! declaring the same controller id summed into one figure.
//!
//! It holds the *route scope* — the value `resolve_route` keys leases by —
//! and deliberately not `api_principal_id`, which is the public API-key ID the
//! span attribute carries. Under `skip_auth` both are `local`; with auth on,
//! the scope is a hash of the credential and the public ID is a database id,
//! so filtering on the wrong one matches nothing exactly where it matters.
//!
//! Nullable, and left null on every existing row: the scope was never
//! recorded, so no backfill can invent one. A pre-migration row therefore
//! matches no principal-scoped query, which is the honest outcome — its
//! attribution is genuinely unknown rather than assumed to be the caller's.

use sea_orm_migration::prelude::*;

#[derive(DeriveMigrationName)]
pub struct Migration;

#[async_trait::async_trait]
impl MigrationTrait for Migration {
    async fn up(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .add_column(ColumnDef::new(Requests::RouteScopeId).string().to_owned())
                    .to_owned(),
            )
            .await
    }

    async fn down(&self, manager: &SchemaManager) -> Result<(), DbErr> {
        manager
            .alter_table(
                Table::alter()
                    .table(Requests::Table)
                    .drop_column(Requests::RouteScopeId)
                    .to_owned(),
            )
            .await
    }
}

#[derive(DeriveIden)]
enum Requests {
    Table,
    RouteScopeId,
}
