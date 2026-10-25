//! Native artifact key custody. Legacy continuation keys and rows are untouched.

use anyhow::{Result, ensure};
use bitrouter_ai::tenancy::identity::InstallationId;
use sea_orm::{
    ConnectionTrait, DatabaseConnection,
    sea_query::{Alias, Expr, Query},
};
use serde::{Serialize, de::DeserializeOwned};

use super::transaction::Control;
use crate::runtime_crypto::{DeploymentKey, SealedSecret};

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

    pub(super) fn seal(
        &self,
        purpose: &str,
        owner: &impl Serialize,
        value: &impl Serialize,
        limit: usize,
    ) -> Result<Vec<u8>> {
        ensure!(
            matches!(purpose, "continuation.native.v1" | "continuation.origin.v1"),
            "invalid native crypto domain"
        );
        let plain = bounded_json(value, limit)?;
        let sealed = self
            .key
            .seal_bound(purpose, &(&self.installation, owner), &plain)?;
        bounded_json(&sealed, limit)
    }

    pub(super) fn open<T: DeserializeOwned>(
        &self,
        purpose: &str,
        owner: &impl Serialize,
        bytes: &[u8],
        limit: usize,
    ) -> Result<T> {
        ensure!(
            matches!(purpose, "continuation.native.v1" | "continuation.origin.v1"),
            "invalid native crypto domain"
        );
        ensure!(bytes.len() <= limit, "native artifact exceeds limit");
        let sealed: SealedSecret = serde_json::from_slice(bytes)?;
        let plain = self
            .key
            .open_bound(purpose, &(&self.installation, owner), &sealed)?;
        ensure!(plain.len() <= limit, "native artifact exceeds limit");
        Ok(serde_json::from_slice(&plain)?)
    }

    pub(super) fn tag_bytes(&self, purpose: &str, bytes: &[u8]) -> Result<String> {
        ensure!(
            purpose == "continuation.candidate.v1",
            "invalid native crypto domain"
        );
        ensure!(bytes.len() <= 1024 * 1024, "native binding exceeds limit");
        self.key
            .fingerprint(self.installation.as_str(), purpose, bytes)
    }

    pub(super) fn tag(&self, purpose: &str, value: &impl Serialize) -> Result<String> {
        ensure!(
            matches!(
                purpose,
                "continuation.credential.v1"
                    | "continuation.route.v1"
                    | "continuation.authority.v1"
            ),
            "invalid native crypto domain"
        );
        // Exact retained target projections are bounded by the compiler envelope.
        let bytes = bounded_json(value, 1024 * 1024)?;
        self.key
            .fingerprint(self.installation.as_str(), purpose, &bytes)
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
    let transaction = control.begin(db).await?;
    let result = async {
        verify_epoch(&transaction, &initial.installation, crypto.as_ref()).await?;
        resources.bootstrap(&transaction, import).await?;
        for (tenant, revision) in &initial.active_configurations {
            let stored = super::configuration_store::configuration(&transaction, tenant, revision)
                .await
                .map_err(|_| anyhow::anyhow!("active tenant configuration unavailable"))?
                .ok_or_else(|| anyhow::anyhow!("active tenant configuration unavailable"))?;
            resources.validate_compiled(&stored).map_err(|_| {
                anyhow::anyhow!("active tenant configuration authentication failed")
            })?;
            super::configuration::validate_profile(&stored.revision.document).map_err(|_| {
                anyhow::anyhow!("active tenant configuration requires unavailable features")
            })?;
        }
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

async fn verify_epoch(
    transaction: &impl ConnectionTrait,
    installation: &InstallationId,
    crypto: Option<&Crypto>,
) -> Result<()> {
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
    } else if let Some(crypto) = crypto {
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
    }
    Ok(())
}
