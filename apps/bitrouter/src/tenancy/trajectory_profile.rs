//! Retained-profile qualification and bounded actor-owned publication preflight.
use anyhow::{Result, ensure};

/// Owned publication has no asynchronous worker. An undelivered row is not a
/// recoverable queue: the complete terminal transaction was required atomically.
/// History validation belongs to an explicit audit, never ordinary startup.
pub(super) async fn validate_audit(
    db: &impl sea_orm::ConnectionTrait,
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    use sea_orm::sea_query::{Alias, Condition, Expr, Query};
    let column = |name| Expr::col(Alias::new(name));
    let incomplete = Condition::any()
        .add(column("tenant_id").is_null())
        .add(column("creator_principal_id").is_null())
        .add(column("logical_identity_json").is_null())
        .add(column("delivered_at").is_null())
        .add(column("attempts").ne(1))
        .add(column("delivered_at").ne(column("created_at")));
    let invalid = Condition::any()
        .add(
            Condition::all()
                .add(column("installation_id").eq(installation.as_str()))
                .add(incomplete),
        )
        .add(
            Condition::all()
                .add(column("installation_id").is_null())
                .add(
                    Condition::any()
                        .add(column("tenant_id").is_not_null())
                        .add(column("creator_principal_id").is_not_null())
                        .add(column("logical_identity_json").is_not_null()),
                ),
        );
    let query = Query::select()
        .expr(Expr::val(1))
        .from(Alias::new("trajectory_outbox"))
        .cond_where(invalid)
        .limit(1)
        .to_owned();
    check()?;
    let invalid = db
        .query_one(db.get_database_backend().build(&query))
        .await?;
    check()?;
    ensure!(
        invalid.is_none(),
        "owned trajectory publication frontier is inconsistent"
    );
    Ok(())
}
