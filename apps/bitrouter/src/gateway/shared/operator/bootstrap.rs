//! One bounded seed transaction, with no partially initialized namespace.

use std::collections::BTreeSet;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::gateway::operator::{
    BootstrapPlan, BootstrapReceipt, Command, ErrorCode, Mutation, PublicationPut,
};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

use super::{Actor, Failure, Operator, commands::Prepared};
use crate::gateway::shared::{
    authority::Authority,
    store::{Kind, NewRecord, Store, Stored},
};

const MAX_BOOTSTRAP_BYTES: usize = 2 * 1024 * 1024;
const MAX_SEED_OPERATIONS: usize = 64;
const BOOTSTRAP_RECORD: &str = "bootstrap";

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Completed {
    fingerprint: String,
    receipt: BootstrapReceipt,
}

/// The caller explicitly grants local bootstrap capabilities. This function
/// validates the complete seed before initializing schema. It never resolves
/// secrets or joins a serving incarnation.
pub(in crate::gateway::shared) async fn initialize(
    authority: Authority,
    actor: &Actor,
    operation_id: &str,
    plan: &BootstrapPlan,
) -> Result<BootstrapReceipt> {
    super::validate_handle(operation_id)?;
    let bytes = serde_json::to_vec(&(&actor.id, operation_id, plan))
        .map_err(|_| Failure(ErrorCode::InvalidInput))?;
    ensure!(
        bytes.len() <= MAX_BOOTSTRAP_BYTES,
        Failure(ErrorCode::InvalidInput)
    );
    let operations = mutations(operation_id, plan).map_err(|_| Failure(ErrorCode::InvalidInput))?;
    for mutation in &operations {
        actor.require(super::required_grant(&mutation.command))?;
    }
    let prepared = operations
        .iter()
        .map(|mutation| Prepared::new(&authority, &actor.id, &mutation.command))
        .collect::<Result<Vec<_>>>()
        .map_err(|_| Failure(ErrorCode::InvalidInput))?;
    let fingerprints = operations
        .iter()
        .map(|mutation| {
            authority.operator_fingerprint("mutation", &serde_json::to_vec(&(&actor.id, mutation))?)
        })
        .collect::<Result<Vec<_>>>()?;
    let fingerprint = authority.operator_fingerprint("bootstrap-plan", &bytes)?;
    let operator = Operator::new(authority.clone());
    let store = authority.store();
    crate::gateway::shared::schema::initialize_all(store)
        .await
        .map_err(|error| super::mutation_failure(&error))?;
    store
        .retry(|| async {
            let tx = store.begin().await?;
            // The exclusive head is acquired before every receipt/domain lock. It
            // remains held through the first publication and the bootstrap receipt.
            let (created, _) = authority.initialize_or_lock(&tx).await?;
            if !created {
                let completed: Stored<Completed> = store
                    .read(&tx, Kind::Authority, BOOTSTRAP_RECORD)
                    .await?
                    .ok_or(Failure(ErrorCode::Conflict))?;
                ensure!(
                    completed.value.fingerprint == fingerprint
                        && completed.value.receipt.namespace == store.namespace()
                        && completed.value.receipt.operator_id == actor.id
                        && completed.value.receipt.operation_id == operation_id,
                    Failure(ErrorCode::Conflict)
                );
                tx.commit().await?;
                return Ok(completed.value.receipt);
            }
            let mut receipts = Vec::with_capacity(operations.len());
            for ((mutation, prepared), fingerprint) in
                operations.iter().zip(&prepared).zip(&fingerprints)
            {
                receipts.push(
                    operator
                        .write(&tx, actor, mutation, prepared, fingerprint)
                        .await?,
                );
            }
            let head = authority.fence_exclusive(&tx).await?;
            ensure!(
                head.value.config_generation == 1 && head.value.key_generation == 1,
                "bootstrap did not publish its first generation"
            );
            let receipt = BootstrapReceipt {
                namespace: store.namespace().into(),
                operator_id: actor.id.clone(),
                operation_id: operation_id.into(),
                committed_at_ms: Store::now(&tx).await?,
                revision: super::commands::revision(&head.value),
                operations: receipts,
            };
            ensure!(
                store
                    .create(
                        &tx,
                        NewRecord {
                            kind: Kind::Authority,
                            key: BOOTSTRAP_RECORD,
                            status: "committed",
                            due_at_ms: None,
                            now_ms: receipt.committed_at_ms,
                            value: &Completed {
                                fingerprint: fingerprint.clone(),
                                receipt: receipt.clone()
                            },
                        }
                    )
                    .await?,
                Failure(ErrorCode::Conflict)
            );
            tx.commit().await?;
            Ok(receipt)
        })
        .await
        .map_err(|error| super::mutation_failure(&error))
}

fn mutations(root: &str, plan: &BootstrapPlan) -> Result<Vec<Mutation>> {
    let count = plan
        .resources
        .len()
        .checked_add(plan.tenants.len())
        .and_then(|count| count.checked_add(plan.principals.len()))
        .and_then(|count| count.checked_add(plan.credentials.len()))
        .and_then(|count| count.checked_add(1))
        .context("bootstrap operation count overflow")?;
    ensure!(
        count <= MAX_SEED_OPERATIONS,
        "bootstrap seed exceeds operation bound"
    );
    let mut resources = BTreeSet::new();
    let mut tenants = BTreeSet::new();
    let mut principals = BTreeSet::new();
    let mut credentials = BTreeSet::new();
    for input in &plan.resources {
        ensure!(
            input.expected_revision.is_none() && resources.insert(input.resource_pool_id.as_str()),
            "bootstrap resources must be unique creates"
        );
    }
    for input in &plan.tenants {
        ensure!(
            input.expected_revision.is_none() && tenants.insert(input.tenant_id.as_str()),
            "bootstrap tenants must be unique creates"
        );
        ensure!(
            input
                .limits
                .resource_pools
                .iter()
                .all(|id| resources.contains(id.as_str())),
            "bootstrap tenant references an unseeded resource"
        );
    }
    for input in &plan.principals {
        ensure!(
            input.expected_authorization_revision.is_none()
                && principals.insert(input.principal_id.as_str()),
            "bootstrap principals must be unique creates"
        );
        ensure!(
            tenants.contains(input.tenant_id.as_str()),
            "bootstrap principal references an unseeded tenant"
        );
    }
    for input in &plan.credentials {
        ensure!(
            input.expected_version.is_none()
                && credentials.insert((input.provider.as_str(), input.account.as_str())),
            "bootstrap credentials must be unique creates"
        );
        ensure!(
            resources.contains(input.resource_pool_id.as_str()),
            "bootstrap credential references an unseeded resource"
        );
    }
    for binding in &plan.configuration.credentials {
        ensure!(
            credentials.contains(&(
                binding.credential_provider.as_str(),
                binding.credential_account.as_str()
            )),
            "bootstrap publication references an unseeded credential"
        );
    }
    let mut commands = Vec::with_capacity(count);
    // Stable order also orders the global principal row locks across concurrent
    // namespace bootstraps that happen to name the same virtual principals.
    let mut inputs: Vec<_> = plan.resources.iter().collect();
    inputs.sort_by_key(|input| &input.resource_pool_id);
    for input in inputs {
        commands.push(Command::PutResource(input.clone()));
    }
    let mut inputs: Vec<_> = plan.tenants.iter().collect();
    inputs.sort_by_key(|input| &input.tenant_id);
    for input in inputs {
        commands.push(Command::PutTenant(input.clone()));
    }
    let mut inputs: Vec<_> = plan.principals.iter().collect();
    inputs.sort_by_key(|input| &input.principal_id);
    for input in inputs {
        commands.push(Command::PutPrincipal(input.clone()));
    }
    let mut inputs: Vec<_> = plan.credentials.iter().collect();
    inputs.sort_by_key(|input| (&input.provider, &input.account));
    for input in inputs {
        commands.push(Command::ImportCredential(input.clone()));
    }
    commands.push(Command::Publish(PublicationPut {
        expected_generation: 0,
        configuration: plan.configuration.clone(),
    }));
    commands
        .into_iter()
        .enumerate()
        .map(|(index, command)| {
            Ok(Mutation {
                operation_id: hex::encode(Sha256::digest(serde_json::to_vec(&(
                    "bootstrap-step-v1",
                    root,
                    index,
                ))?)),
                command,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests;
