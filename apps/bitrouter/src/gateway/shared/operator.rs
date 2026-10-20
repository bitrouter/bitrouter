//! Explicit administration without a serving incarnation or provider execution.

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::operator::{
    Command, CredentialMetadata, ErrorCode, Grant, Mutation, OperationLookup, Outcome,
    PrincipalMetadata, Receipt, ResourcePolicy, Revision, TenantPolicy,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::authority::Authority;
use super::engine::{Rejection, management};
use super::store::{Kind, NewRecord, Store, Stored};

pub(super) mod bootstrap;
mod commands;
mod principals;

const MAX_MUTATION_BYTES: usize = 2 * 1024 * 1024;

/// Constructed by explicit local bootstrap or the separate operator authenticator.
/// Inference authentication never produces this capability.
pub(super) struct Actor {
    id: String,
    grants: BTreeSet<Grant>,
}

impl Actor {
    pub fn new(id: String, grants: BTreeSet<Grant>) -> Result<Self> {
        validate_handle(&id)?;
        ensure!(!grants.is_empty(), Failure(ErrorCode::Forbidden));
        Ok(Self { id, grants })
    }

    fn require(&self, grant: Grant) -> Result<()> {
        ensure!(self.grants.contains(&grant), Failure(ErrorCode::Forbidden));
        Ok(())
    }
}

#[derive(Debug, thiserror::Error)]
#[error("shared operator request rejected ({0:?})")]
pub(super) struct Failure(pub ErrorCode);

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    fingerprint: String,
    grant: Grant,
    receipt: Option<Receipt>,
}

#[derive(Clone)]
pub(super) struct Operator {
    authority: Authority,
}

impl Operator {
    pub fn new(authority: Authority) -> Self {
        Self { authority }
    }

    pub async fn apply(&self, actor: &Actor, mutation: &Mutation) -> Result<Receipt> {
        let grant = required_grant(&mutation.command);
        actor.require(grant)?;
        validate_handle(&mutation.operation_id)?;
        let bytes = serde_json::to_vec(&(&actor.id, mutation))
            .map_err(|_| Failure(ErrorCode::InvalidInput))?;
        ensure!(
            bytes.len() <= MAX_MUTATION_BYTES,
            Failure(ErrorCode::InvalidInput)
        );
        let prepared = commands::Prepared::new(&self.authority, &actor.id, &mutation.command)
            .map_err(|_| Failure(ErrorCode::InvalidInput))?;
        let fingerprint = self.authority.operator_fingerprint("mutation", &bytes)?;
        let store = self.authority.store();
        let result = store
            .retry(|| async {
                let tx = store.begin().await?;
                let receipt = self
                    .write(&tx, actor, mutation, &prepared, &fingerprint)
                    .await?;
                tx.commit().await?;
                Ok(receipt)
            })
            .await;
        // A backend error may be a lost COMMIT acknowledgement. Preserve the
        // caller's operation identity, and never classify it as a failed write.
        result.map_err(|error| mutation_failure(&error))
    }

    /// Complete the mutation and receipt before the caller commits. This helper
    /// never opens a second transaction or acquires a serving incarnation.
    async fn write(
        &self,
        tx: &sea_orm::DatabaseTransaction,
        actor: &Actor,
        mutation: &Mutation,
        prepared: &commands::Prepared<'_>,
        fingerprint: &str,
    ) -> Result<Receipt> {
        let store = self.authority.store();
        let grant = required_grant(&mutation.command);
        actor.require(grant)?;
        let key = operation_key(&actor.id, &mutation.operation_id)?;
        // Lock order: authority head, operation receipt, then the affected
        // tenant/resource. Inference never acquires an operator receipt.
        let mut head = if prepared.exclusive() {
            self.authority.fence_exclusive(tx).await?
        } else {
            self.authority.fence(tx).await?
        };
        let now = Store::now(tx).await?;
        let created = store
            .create(
                tx,
                NewRecord {
                    kind: Kind::OperatorReceipt,
                    key: &key,
                    status: "pending",
                    due_at_ms: None,
                    now_ms: now,
                    value: &Record {
                        fingerprint: fingerprint.to_owned(),
                        grant,
                        receipt: None,
                    },
                },
            )
            .await?;
        let mut record: Stored<Record> = store.lock(tx, Kind::OperatorReceipt, &key).await?;
        ensure!(
            record.value.fingerprint == fingerprint && record.value.grant == grant,
            Failure(ErrorCode::Conflict)
        );
        if !created {
            let receipt = checked_receipt(
                &record.value,
                store.namespace(),
                actor,
                &mutation.operation_id,
            )?;
            return Ok(receipt);
        }
        let outcome = prepared
            .apply(&self.authority, tx, &mut head, &actor.id)
            .await?;
        let receipt = Receipt {
            namespace: store.namespace().into(),
            operator_id: actor.id.clone(),
            operation_id: mutation.operation_id.clone(),
            committed_at_ms: Store::now(tx).await?,
            outcome,
        };
        record.value.receipt = Some(receipt.clone());
        store
            .save(tx, &mut record, "committed", None, receipt.committed_at_ms)
            .await?;

        Ok(receipt)
    }

    pub async fn lookup(&self, actor: &Actor, operation_id: &str) -> Result<OperationLookup> {
        validate_handle(operation_id)?;
        let key = operation_key(&actor.id, operation_id)?;
        let store = self.authority.store();
        store
            .bounded(async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let record: Option<Stored<Record>> =
                    store.read(&tx, Kind::OperatorReceipt, &key).await?;
                let result = match record {
                    Some(record) => {
                        actor.require(record.value.grant)?;
                        OperationLookup::Committed {
                            receipt: Box::new(checked_receipt(
                                &record.value,
                                store.namespace(),
                                actor,
                                operation_id,
                            )?),
                        }
                    }
                    None => OperationLookup::Unresolved {
                        operation_id: operation_id.into(),
                    },
                };
                tx.commit().await?;
                Ok(result)
            })
            .await
            .map_err(|error| read_failure(&error))
    }

    pub async fn tenant(&self, actor: &Actor, id: &str) -> Result<TenantPolicy> {
        actor.require(Grant::Read)?;
        super::engine::validate_id(id).map_err(|_| Failure(ErrorCode::InvalidInput))?;
        let store = self.authority.store();
        store
            .bounded(async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let result = management::tenant(&self.authority, &tx, id).await?;
                tx.commit().await?;
                Ok(result)
            })
            .await
            .map_err(|error| read_failure(&error))
    }

    pub async fn resource(&self, actor: &Actor, id: &str) -> Result<ResourcePolicy> {
        actor.require(Grant::Read)?;
        super::engine::validate_id(id).map_err(|_| Failure(ErrorCode::InvalidInput))?;
        let store = self.authority.store();
        store
            .bounded(async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let result = management::resource(&self.authority, &tx, id).await?;
                tx.commit().await?;
                Ok(result)
            })
            .await
            .map_err(|error| read_failure(&error))
    }
    pub async fn revision(&self, actor: &Actor) -> Result<Revision> {
        actor.require(Grant::Read)?;
        self.authority
            .head()
            .await
            .map(|head| commands::revision(&head))
            .map_err(|error| read_failure(&error))
    }

    /// A global principal read needs the same explicit global capability as its
    /// writer; namespace Read does not grant access to another tenant's keys.
    pub async fn principal(&self, actor: &Actor, id: &str) -> Result<PrincipalMetadata> {
        actor.require(Grant::GlobalPrincipalWrite)?;
        super::engine::validate_id(id).map_err(|_| Failure(ErrorCode::InvalidInput))?;
        let store = self.authority.store();
        store
            .bounded(async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let result = principals::read(&self.authority, &tx, id).await?;
                tx.commit().await?;
                Ok(result)
            })
            .await
            .map_err(|error| read_failure(&error))
    }

    pub async fn credential(
        &self,
        actor: &Actor,
        provider: &str,
        account: &str,
    ) -> Result<CredentialMetadata> {
        actor.require(Grant::Read)?;
        for id in [provider, account] {
            super::engine::validate_id(id).map_err(|_| Failure(ErrorCode::InvalidInput))?;
        }
        let slot = crate::gateway::providers::auth::store::CredentialKey {
            provider: provider.into(),
            account: account.into(),
        };
        let store = self.authority.store();
        store
            .bounded(async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let stamp = super::credentials::Credentials::new(self.authority.clone())
                    .metadata_in(&tx, &slot)
                    .await?;
                tx.commit().await?;
                Ok(CredentialMetadata {
                    provider: provider.into(),
                    account: account.into(),
                    resource_pool_id: stamp.pool_id,
                    version: stamp.version,
                })
            })
            .await
            .map_err(|error| read_failure(&error))
    }

    pub async fn execution(
        &self,
        actor: &Actor,
        id: &str,
    ) -> Result<bitrouter_ai::gateway::execution::GatewayExecution> {
        actor.require(Grant::Read)?;
        super::engine::validate_id(id).map_err(|_| Failure(ErrorCode::InvalidInput))?;
        let store = self.authority.store();
        store
            .bounded(async {
                let tx = store.begin().await?;
                self.authority.fence(&tx).await?;
                let view = management::execution(&self.authority, &tx, id).await?;
                tx.commit().await?;
                Ok(view)
            })
            .await
            .map_err(|error| read_failure(&error))
    }
}

fn checked_receipt(
    record: &Record,
    namespace: &str,
    actor: &Actor,
    operation_id: &str,
) -> Result<Receipt> {
    let receipt = record
        .receipt
        .as_ref()
        .context("operator receipt is incomplete")?;
    ensure!(
        receipt.namespace == namespace
            && receipt.operator_id == actor.id
            && receipt.operation_id == operation_id,
        "operator receipt binding changed"
    );
    let grant = match &receipt.outcome {
        Outcome::Tenant(_) | Outcome::Resource(_) => Grant::PolicyWrite,
        Outcome::Principal(_) => Grant::GlobalPrincipalWrite,
        Outcome::Credential(_) => Grant::CredentialWrite,
        Outcome::Published(_) => Grant::Publish,
        Outcome::CorrelationRotated(_) => Grant::RotateCorrelation,
        Outcome::Execution(_) => Grant::ResolveExecution,
    };
    ensure!(record.grant == grant, "operator receipt grant changed");
    Ok(receipt.clone())
}

pub(super) fn validate_handle(value: &str) -> Result<()> {
    ensure!(
        !value.is_empty()
            && value.len() <= 128
            && value
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.')),
        Failure(ErrorCode::InvalidInput)
    );
    Ok(())
}

fn operation_key(actor: &str, operation_id: &str) -> Result<String> {
    Ok(hex::encode(Sha256::digest(serde_json::to_vec(&(
        actor,
        operation_id,
    ))?)))
}

fn required_grant(command: &Command) -> Grant {
    match command {
        Command::PutTenant(_) | Command::PutResource(_) => Grant::PolicyWrite,
        Command::PutPrincipal(_) => Grant::GlobalPrincipalWrite,
        Command::ImportCredential(_) => Grant::CredentialWrite,
        Command::Publish(_) => Grant::Publish,
        Command::RotateCorrelation { .. } => Grant::RotateCorrelation,
        Command::ResolveExecution { .. } => Grant::ResolveExecution,
    }
}

fn mutation_failure(error: &anyhow::Error) -> anyhow::Error {
    if let Some(Failure(code)) = error.downcast_ref::<Failure>() {
        return Failure(*code).into();
    }
    if matches!(error.downcast_ref::<Rejection>(), Some(Rejection::NotFound)) {
        return Failure(ErrorCode::NotFound).into();
    }
    if matches!(
        error.downcast_ref::<Rejection>(),
        Some(Rejection::AuthorityChanged)
    ) {
        return Failure(ErrorCode::Conflict).into();
    }
    Failure(ErrorCode::Unknown).into()
}

fn read_failure(error: &anyhow::Error) -> anyhow::Error {
    if let Some(Failure(code)) = error.downcast_ref::<Failure>() {
        return Failure(*code).into();
    }
    if matches!(error.downcast_ref::<Rejection>(), Some(Rejection::NotFound)) {
        return Failure(ErrorCode::NotFound).into();
    }
    Failure(ErrorCode::Unavailable).into()
}

#[cfg(test)]
mod tests;
