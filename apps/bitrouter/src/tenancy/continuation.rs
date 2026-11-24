//! Tenant continuation key custody and acknowledged bootstrap.

pub(crate) mod http;

use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::identity::InstallationId;
use sea_orm::{
    ConnectionTrait, DatabaseConnection,
    sea_query::{Alias, Expr, Query},
};
use serde::Serialize;

use super::transaction::Control;
use crate::runtime_crypto::DeploymentKey;

#[cfg(test)]
mod tests;

pub(super) const EPOCH_TABLE: &str = "tenancy_continuation_epoch";

pub(super) async fn has_epoch(db: &impl ConnectionTrait) -> Result<bool> {
    let query = Query::select()
        .expr(Expr::val(1_i32))
        .from(Alias::new(EPOCH_TABLE))
        .limit(1)
        .to_owned();
    Ok(db
        .query_one(db.get_database_backend().build(&query))
        .await?
        .is_some())
}

/// Constructed only after acknowledged commit of the actor's whole bootstrap.
#[derive(Clone)]
pub(super) struct PinnedCrypto(Crypto);

impl std::ops::Deref for PinnedCrypto {
    type Target = Crypto;
    fn deref(&self) -> &Self::Target {
        &self.0
    }
}

/// A clone of the explicitly configured key, never an ambient key lookup.
#[derive(Clone)]
pub(super) struct Crypto {
    installation: InstallationId,
    key: DeploymentKey,
}

impl Crypto {
    pub(super) fn new(installation: InstallationId, key: DeploymentKey) -> Self {
        Self { installation, key }
    }

    pub(super) fn id(&self) -> &str {
        self.key.id()
    }
}

/// Check before extending, including serialization's intermediate output.
pub(super) fn bounded_json(value: &impl Serialize, limit: usize) -> Result<Vec<u8>> {
    struct Output {
        bytes: Vec<u8>,
        limit: usize,
    }
    impl std::io::Write for Output {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            if bytes.len() > self.limit.saturating_sub(self.bytes.len()) {
                return Err(std::io::Error::other("native artifact exceeds limit"));
            }
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let mut output = Output {
        bytes: Vec::new(),
        limit,
    };
    serde_json::to_writer(&mut output, value)?;
    Ok(output.bytes)
}

/// Called only by the existing owning actor, under its transaction supervisor.
pub(super) async fn initialize(
    db: &DatabaseConnection,
    initial: &super::model::Snapshot,
    resources: &super::resources::Resources,
    import: Option<&[u8]>,
    control: &Control,
) -> Result<Option<PinnedCrypto>> {
    let crypto = resources.continuation_crypto();
    // Import expansion and its bounded metadata/index rows share the same
    // physical ledger. With no import, only the epoch and its required flag
    // can be written in their two small metadata pages.
    let bytes = match import {
        Some(bytes) => bytes
            .len()
            .checked_mul(2)
            .and_then(|n| n.checked_add(4 * 1024 * 1024))
            .ok_or_else(|| anyhow::anyhow!("bootstrap write estimate overflow"))?,
        None => 8192,
    };
    control
        .reserve_write(bytes, true)
        .map_err(|code| anyhow::anyhow!("bootstrap storage capacity unavailable: {code:?}"))?;
    let transaction = control.begin_write(db).await?;
    let result = async {
        verify_epoch(&transaction, &initial.installation, crypto.as_ref()).await?;
        resources.bootstrap(&transaction, import).await?;
        Ok::<_, anyhow::Error>(())
    }
    .await;
    match result {
        Ok(()) => {
            control.commit(transaction).await?;
            Ok(crypto.map(PinnedCrypto))
        }
        Err(error) => {
            control.rollback(transaction).await?;
            Err(error)
        }
    }
}

/// Complete read-only authentication under the caller's stable audit snapshot.
/// Ordinary startup validates only the epoch and explicitly imported resources.
pub(in crate::tenancy) async fn audit_in(
    db: &impl ConnectionTrait,
    header: &super::model::AuthorityHeader,
    resources: &super::resources::Resources,
    bytes: usize,
    check: &(dyn Fn() -> Result<()> + Sync),
) -> Result<()> {
    check()?;
    let crypto = resources.continuation_crypto();
    ensure!(
        check_epoch(db, &header.installation, crypto.as_ref()).await?
            || !crate::db::migration::capacity::epoch_required(db).await?,
        "native continuation epoch missing"
    );
    super::trajectory_profile::validate_audit(db, &header.installation, check).await?;
    http::bootstrap::validate_audit(db, &header.installation, crypto.as_ref(), check).await?;
    resources.audit_retained_in(db, bytes, check).await?;
    let mut after = None;
    loop {
        check()?;
        let page = super::configuration_store::active_revision_page(db, after.as_ref()).await?;
        if page.is_empty() {
            break;
        }
        for pointer in page {
            check()?;
            ensure!(
                pointer.authority_revision <= header.revision,
                "active configuration exceeds published authority"
            );
            let stored =
                super::configuration_store::configuration(db, &pointer.tenant, &pointer.revision)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("active tenant configuration unavailable"))?;
            resources.validate_executable(&stored).map_err(|_| {
                anyhow::anyhow!(
                    "active tenant configuration authentication or qualification failed"
                )
            })?;
            super::configuration::validate_profile(&stored.revision.document).map_err(|_| {
                anyhow::anyhow!("active tenant configuration requires unavailable features")
            })?;
            after = Some(pointer.tenant);
        }
    }
    resources.audit_oauth_in(db, check).await?;
    check()
}

async fn check_epoch(
    transaction: &impl ConnectionTrait,
    installation: &InstallationId,
    crypto: Option<&Crypto>,
) -> Result<bool> {
    let mut query = Query::select();
    query
        .from(Alias::new(EPOCH_TABLE))
        .column(Alias::new("singleton"))
        .expr_as(
            Expr::cust(
                "CASE WHEN length(installation_id) <= 128 THEN installation_id ELSE NULL END",
            ),
            Alias::new("installation_id"),
        )
        .expr_as(
            Expr::cust("CASE WHEN length(key_id) = 64 THEN key_id ELSE NULL END"),
            Alias::new("key_id"),
        )
        .limit(2);
    let rows = transaction
        .query_all(transaction.get_database_backend().build(&query))
        .await?;
    ensure!(rows.len() <= 1, "invalid native continuation epoch");
    if let Some(row) = rows.first() {
        let owner: Option<String> = row.try_get("", "installation_id")?;
        let key: Option<String> = row.try_get("", "key_id")?;
        ensure!(
            row.try_get::<i32>("", "singleton")? == 1
                && owner.as_deref() == Some(installation.as_str())
                && crypto.is_some_and(|crypto| key.as_deref() == Some(crypto.id())),
            "native continuation key epoch unavailable"
        );
        return Ok(true);
    }
    Ok(false)
}

async fn verify_epoch(
    transaction: &impl ConnectionTrait,
    installation: &InstallationId,
    crypto: Option<&Crypto>,
) -> Result<()> {
    if check_epoch(transaction, installation, crypto).await? {
        return crate::db::migration::capacity::mark_epoch_required(transaction).await;
    }
    ensure!(
        !crate::db::migration::capacity::epoch_required(transaction).await?,
        "native continuation epoch missing"
    );
    if let Some(crypto) = crypto {
        let mut insert = Query::insert();
        insert
            .into_table(Alias::new(EPOCH_TABLE))
            .columns(["singleton", "installation_id", "key_id", "created_at"].map(Alias::new))
            .values([
                1_i32.into(),
                installation.as_str().into(),
                crypto.id().into(),
                chrono::Utc::now()
                    .to_rfc3339_opts(chrono::SecondsFormat::Secs, true)
                    .into(),
            ])?;
        transaction
            .execute(transaction.get_database_backend().build(&insert))
            .await?;
        crate::db::migration::capacity::mark_epoch_required(transaction).await?;
    }
    Ok(())
}
