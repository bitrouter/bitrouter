//! Storage-boundary regressions, not authorization-server claims.

use anyhow::{Context, Result, ensure};
use bitrouter::db::migration::Migrator;
use bitrouter_ai::tenancy::{
    authority::{GrantScope, OwnerKind, ResourceKey, ResourceKind},
    validation::Validate,
};
use sea_orm::{
    ConnectionTrait, DatabaseConnection, Value,
    sea_query::{Alias, Expr, Query},
};
use sea_orm_migration::{MigratorTrait, SchemaManager};

async fn insert(
    db: &DatabaseConnection,
    table: &str,
    columns: &[&str],
    values: Vec<Value>,
) -> Result<()> {
    let mut query = Query::insert();
    query
        .into_table(Alias::new(table))
        .columns(columns.iter().map(|column| Alias::new(*column)));
    query.values(values.into_iter().map(Into::into))?;
    db.execute(db.get_database_backend().build(&query)).await?;
    Ok(())
}

async fn scope(db: &DatabaseConnection, tenant: &str, binding: &str) -> Result<String> {
    let query = Query::select()
        .column(Alias::new("scope_json"))
        .from(Alias::new("tenancy_bindings"))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(tenant))
        .and_where(Expr::col(Alias::new("id")).eq(binding))
        .to_owned();
    let rows = db
        .query_all(db.get_database_backend().build(&query))
        .await?;
    ensure!(rows.len() == 1, "owner lookup must be exact");
    Ok(rows[0].try_get("", "scope_json")?)
}

pub(super) async fn exercise(db: &DatabaseConnection) -> Result<()> {
    let suffix = uuid::Uuid::new_v4().to_string();
    let upper = format!("Tenant-{suffix}");
    let lower = format!("tenant-{suffix}");
    for id in [&upper, &lower] {
        insert(
            db,
            "tenancy_tenants",
            &["id", "revision", "state", "created_at"],
            vec![
                id.into(),
                "revision".into(),
                "active".into(),
                "2026-01-01T00:00:00Z".into(),
            ],
        )
        .await?;
    }
    let principal = format!("principal-{suffix}");
    let issuer = format!("issuer-{suffix}");
    for (index, subject) in ["Subject", "subject", "subject ", "é", "e\u{301}"]
        .into_iter()
        .enumerate()
    {
        insert(
            db,
            "tenancy_principals",
            &["id", "revision", "issuer", "subject", "created_at"],
            vec![
                format!("{principal}-{index}").into(),
                "revision".into(),
                issuer.clone().into(),
                subject.into(),
                "2026-01-01T00:00:00Z".into(),
            ],
        )
        .await?;
    }
    // The largest valid resource keys exceed MySQL TEXT's 64 KiB ceiling.
    let resources = (0..256)
        .map(|index| {
            Ok(ResourceKey {
                owner_kind: OwnerKind::Tenant,
                owner_id: "o".repeat(128).try_into()?,
                resource_kind: ResourceKind::Route,
                resource_id: format!("{index:0>128}").try_into()?,
            })
        })
        .collect::<Result<Vec<_>>>()?;
    let grant = GrantScope::Resources { resources };
    grant.validate()?;
    let wire = serde_json::to_string(&grant)?;
    ensure!(
        wire.len() > 65_535,
        "fixture must exercise the old MySQL text ceiling"
    );
    let binding = format!("binding-{suffix}");
    for (tenant, scope_json) in [(&upper, wire.as_str()), (&lower, r#"{"kind":"tenant"}"#)] {
        insert(
            db,
            "tenancy_bindings",
            &[
                "tenant_id",
                "id",
                "principal_id",
                "revision",
                "role",
                "scope_json",
                "expires_at",
                "management_owner",
                "management_class",
            ],
            vec![
                tenant.into(),
                binding.clone().into(),
                format!("{principal}-0").into(),
                "revision".into(),
                "viewer".into(),
                scope_json.into(),
                "2030-01-01T00:00:00Z".into(),
                Option::<String>::None.into(),
                Option::<String>::None.into(),
            ],
        )
        .await?;
    }
    ensure!(
        scope(db, &upper, &binding).await? == wire,
        "large grant did not round trip"
    );
    ensure!(
        scope(db, &lower, &binding).await? == r#"{"kind":"tenant"}"#,
        "case-distinct owner was merged"
    );
    let migrations = Migrator::migrations();
    let migration = migrations
        .iter()
        .find(|m| m.name() == "m20240101_000028_resource_tenancy")
        .context("resource tenancy migration absent")?;
    // Simulate DDL having committed before its migration-ledger update.
    migration.up(&SchemaManager::new(db)).await?;
    ensure!(
        scope(db, &upper, &binding).await? == wire,
        "migration resume changed retained scope"
    );
    Ok(())
}
