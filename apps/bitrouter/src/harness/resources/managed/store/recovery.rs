//! Full native row checks shared by preparation and the actual checkpoint writer.
use super::local::Journal;
use crate::{
    storage::checkpoint::CheckpointBatchExt,
    tenancy::{
        execution::session::Binding,
        receipts::recovery::Seed,
        session::recovery::{Budget, CheckedRecovery},
    },
};
use anyhow::{Context, Result, ensure};
use std::collections::{BTreeMap, BTreeSet};

pub(crate) struct Source {
    pub(crate) workspace: std::path::PathBuf,
    pub(crate) grant: bitrouter_ai::core::protocol::OwnershipGrant,
    pub(crate) seeds: Vec<Seed>,
    pub(crate) steps: BTreeMap<String, Vec<bitrouter_ai::tenancy::identity::ExecutionId>>,
    pub(crate) pending_steps: BTreeSet<String>,
}
pub(crate) fn validate(bytes: &[u8], binding: &Binding, budget: &Budget) -> Result<Source> {
    budget.charge(0, bytes.len())?;
    let journal: Journal = serde_json::from_slice(bytes)?;
    ensure!(
        journal.format == 1
            && !journal.released
            && journal.starts.is_empty()
            && journal.fences.is_empty(),
        "native stopped recovery profile mismatch"
    );
    binding
        .grant(&journal.grant)
        .map_err(|_| anyhow::anyhow!("native grant mismatch"))?;
    super::choice::validate(&journal, Some(binding))
        .map_err(|_| anyhow::anyhow!("native Choice linkage mismatch"))?;
    let batch = journal
        .checkpoint
        .as_ref()
        .context("native checkpoint missing")?;
    let payload = batch
        .decode(&journal.limits)
        .map_err(|_| anyhow::anyhow!("invalid native checkpoint"))?;
    let ack = crate::storage::checkpoint::ack_for_batch(batch, &payload);
    ensure!(
        journal.ack.as_ref() == Some(&ack) && journal.head == ack.head(),
        "native whole-row ACK mismatch"
    );
    ensure!(
        batch.identity.session_id == journal.grant.session_id
            && batch.identity.execution_epoch == journal.grant.execution_epoch
            && batch.identity.core_instance_id == journal.grant.core_instance_id,
        "native checkpoint owner mismatch"
    );
    let mut visited = BTreeSet::new();
    for reference in journal.available() {
        budget.charge(0, usize::try_from(reference.bytes)?)?;
        journal
            .require_artifact(&reference, &mut visited)
            .map_err(|_| anyhow::anyhow!("native artifact integrity"))?;
    }
    let mut seeds: BTreeMap<_, Seed> = BTreeMap::new();
    let mut steps: BTreeMap<String, Vec<bitrouter_ai::tenancy::identity::ExecutionId>> =
        BTreeMap::new();
    let mut pending_steps = BTreeSet::new();
    for record in journal.owned_choices.values() {
        ensure!(record.version == 2, "native Choice has no admission seeds");
        let (parent, child) = record
            .seeds
            .as_ref()
            .context("native Choice seeds missing")?;
        for seed in [parent, child] {
            if let Some(prior) = seeds.insert(seed.execution_id().clone(), seed.clone()) {
                ensure!(&prior == seed, "conflicting receipt admission seeds");
            }
        }
        let parents = steps.entry(record.step_id.clone()).or_default();
        if !parents.contains(&record.parent_execution) {
            parents.push(record.parent_execution.clone());
        }
        if record.outcome_digest.is_none() || record.terminal_digests.is_some() {
            pending_steps.insert(record.step_id.clone());
        }
    }
    Ok(Source {
        workspace: journal.workspace,
        grant: journal.grant,
        seeds: seeds.into_values().collect(),
        steps,
        pending_steps,
    })
}

pub(in crate::harness::resources::managed) fn apply_links(
    journal: &mut Journal,
    checked: &CheckedRecovery,
) -> Result<()> {
    for record in journal.owned_choices.values_mut() {
        let digests =
            checked.terminal_digests(&record.parent_execution, &record.child_execution)?;
        if let Some(prior) = &record.terminal_digests {
            ensure!(prior == &digests, "native terminal linkage changed");
        }
        record.terminal_digests = Some(digests);
    }
    Ok(())
}

pub(crate) fn validate_terminal_links(bytes: &[u8], checked: &CheckedRecovery) -> Result<()> {
    checked.budget.charge(0, bytes.len())?;
    let journal: Journal = serde_json::from_slice(bytes)?;
    for record in journal.owned_choices.values() {
        if let Some(stored) = &record.terminal_digests {
            ensure!(
                stored
                    == &checked
                        .terminal_digests(&record.parent_execution, &record.child_execution)?,
                "stored native terminal digest mismatch"
            );
        }
    }
    Ok(())
}

pub(crate) fn validate_replacement(
    previous: &[u8],
    bytes: &[u8],
    binding: &Binding,
    checked: &CheckedRecovery,
) -> Result<()> {
    checked.budget.charge(
        0,
        previous
            .len()
            .checked_add(bytes.len())
            .context("native proposal bound")?,
    )?;
    let previous: Journal = serde_json::from_slice(previous)?;
    let next: Journal = serde_json::from_slice(bytes)?;
    binding
        .grant(&next.grant)
        .map_err(|_| anyhow::anyhow!("native recovery grant mismatch"))?;
    ensure!(
        next.grant.session_id == previous.grant.session_id
            && next.grant.harness_id == previous.grant.harness_id
            && Some(next.grant.execution_epoch) == previous.grant.execution_epoch.checked_add(1)
            && next.workspace == previous.workspace
            && next.mode == previous.mode
            && !next.released
            && next.starts.is_empty()
            && next.fences.is_empty(),
        "native recovery replacement identity"
    );
    let batch = next
        .checkpoint
        .as_ref()
        .context("native recovery checkpoint missing")?;
    let payload = batch
        .decode(&next.limits)
        .map_err(|_| anyhow::anyhow!("native recovery checkpoint invalid"))?;
    ensure!(
        payload.base_state_revision == previous.head.state_revision
            && payload.base_event_seq == previous.head.event_seq
            && batch.identity.execution_epoch == next.grant.execution_epoch
            && batch.identity.core_instance_id == next.grant.core_instance_id
            && batch.identity.session_id == next.grant.session_id
            && payload
                .events
                .iter()
                .any(|event| event.kind == "session.restored"),
        "native recovery requires its complete first restore checkpoint"
    );
    let ack = crate::storage::checkpoint::ack_for_batch(batch, &payload);
    ensure!(
        next.ack.as_ref() == Some(&ack) && next.head == ack.head(),
        "native recovery ACK mismatch"
    );
    ensure!(
        next.owned_choices.len() == previous.owned_choices.len(),
        "native recovery changed intent inventory"
    );
    for (id, old) in &previous.owned_choices {
        let current = next
            .owned_choices
            .get(id)
            .context("native recovery dropped an intent")?;
        ensure!(
            current.terminal_digests.as_ref()
                == Some(
                    &checked
                        .terminal_digests(&current.parent_execution, &current.child_execution)?
                ),
            "native recovery terminal links missing"
        );
        let mut immutable = current.clone();
        immutable.outcome_digest = old.outcome_digest.clone();
        immutable.terminal_digests = old.terminal_digests.clone();
        ensure!(
            &immutable == old,
            "native recovery changed immutable intent"
        );
    }
    super::choice::validate(&next, Some(binding))
        .map_err(|_| anyhow::anyhow!("native recovery outcome linkage mismatch"))?;
    Ok(())
}

/// The installation-wide safety scan validates released peers without turning
/// another principal's bytes into a usable Binding or returning their content.
pub(crate) fn validate_released_owner(
    bytes: &[u8],
    descriptor: &crate::tenancy::execution::session::Descriptor,
) -> Result<()> {
    let journal: Journal = serde_json::from_slice(bytes)?;
    journal
        .grant
        .validate()
        .map_err(|_| anyhow::anyhow!("native peer grant invalid"))?;
    ensure!(
        journal.format == 1
            && journal.released
            && journal.grant.session_id == descriptor.session
            && journal.grant.harness_id == descriptor.harness,
        "native peer release owner invalid"
    );
    let batch = journal
        .checkpoint
        .as_ref()
        .context("native peer release checkpoint absent")?;
    let payload = batch
        .decode(&journal.limits)
        .map_err(|_| anyhow::anyhow!("native peer checkpoint invalid"))?;
    let ack = crate::storage::checkpoint::ack_for_batch(batch, &payload);
    ensure!(
        journal.ack.as_ref() == Some(&ack)
            && journal.head == ack.head()
            && batch.identity.execution_epoch == journal.grant.execution_epoch
            && batch.identity.core_instance_id == journal.grant.core_instance_id
            && batch.identity.session_id == journal.grant.session_id,
        "native peer release checkpoint mismatch"
    );
    Ok(())
}
