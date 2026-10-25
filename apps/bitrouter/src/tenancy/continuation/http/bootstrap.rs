//! Bounded, read-only preflight in the existing H bootstrap, before any import.
use super::super::{Crypto, has_epoch};
use crate::tenancy::transaction::Control;
use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::identity::InstallationId;
use sea_orm::{
    ConnectionTrait,
    sea_query::{Alias, Condition, Expr, Query},
};
fn column(name: &str) -> Expr {
    Expr::col(Alias::new(name))
}
fn any_owned() -> Condition {
    Condition::any()
        .add(column("installation_id").is_not_null())
        .add(column("tenant_id").is_not_null())
        .add(column("creator_principal_id").is_not_null())
        .add(column("cipher_version").ne(super::codec::VERSION - 1))
}
async fn exists(db: &impl ConnectionTrait, condition: Condition) -> Result<bool> {
    let query = Query::select()
        .expr(Expr::val(1_i32))
        .from(Alias::new("provider_continuations"))
        .cond_where(condition)
        .limit(1)
        .to_owned();
    Ok(db
        .query_one(db.get_database_backend().build(&query))
        .await?
        .is_some())
}
pub(in crate::tenancy) async fn has_owned(db: &impl ConnectionTrait) -> Result<bool> {
    exists(db, any_owned()).await
}
pub(in crate::tenancy) async fn validate(
    db: &impl ConnectionTrait,
    installation: &InstallationId,
    crypto: Option<&Crypto>,
    control: &Control,
) -> Result<()> {
    control.check_read()?;
    let owned = has_owned(db).await?;
    control.check_read()?;
    if !owned {
        return Ok(());
    }
    let crypto = crypto.ok_or_else(|| anyhow::anyhow!("HTTP continuation key unavailable"))?;
    // Never initialize a replacement epoch around retained HTTP rows, even if
    // resources and the active-configuration catalog happen to be empty.
    ensure!(has_epoch(db).await?, "HTTP continuation epoch missing");
    control.check_read()?;
    let incomplete = Condition::any()
        .add(column("installation_id").is_null())
        .add(column("tenant_id").is_null())
        .add(column("creator_principal_id").is_null())
        .add(
            Expr::expr(super::store::exact(
                db.get_database_backend(),
                "installation_id",
                installation.as_str(),
            ))
            .not(),
        )
        .add(column("cipher_version").ne(super::codec::VERSION))
        .add(
            Expr::expr(super::store::exact(
                db.get_database_backend(),
                "key_id",
                crypto.id(),
            ))
            .not(),
        )
        .add(column("nonce").is_not_null())
        .add(column("ciphertext").is_null());
    ensure!(
        !exists(db, Condition::all().add(any_owned()).add(incomplete)).await?,
        "HTTP continuation provenance unavailable"
    );
    control.check_read()?;
    Ok(())
}
