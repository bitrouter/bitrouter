//! One borrowed actor transaction stamps the closed Registry's exact paid set.
use super::*;
use crate::tenancy::{
    execution::lifecycle::paid::StopSet,
    learning::recovery::{Member, Stopped},
    transaction::Proof,
};
use std::sync::Arc;

pub(in crate::tenancy) async fn retain(
    tx: &DatabaseTransaction,
    proof: &StopSet,
    owner: &Arc<Proof>,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let mut members = Vec::new();
    let first = proof.binding();
    let origin = first
        .service()
        .startup
        .as_ref()
        .context("paid actual origin absent")?
        .stamp(owner)?;
    for member in proof.owners() {
        ensure!(
            members.len() < 8 && member.binding.origin(owner),
            "paid stop set owner mismatch"
        );
        let seed = member.recovery.as_ref().context("paid seed absent")?;
        ensure!(seed.origin == origin, "paid stop set acquisition mismatch");
        members.push(Member::from_seed(seed)?);
    }
    let set_digest = crate::storage::evolution::digest(&(&origin, &members))?;
    // Every member is checked before the first update. The same H transaction
    // later CASes both mirrors; no waiter can attest that these writes settled.
    let mut writes = Vec::new();
    let mut native_writes = Vec::new();
    for member in proof.owners() {
        let binding = match &member.binding {
            crate::tenancy::learning::source::Binding::Native(_) => {
                if let Some(write) = super::native::cleanup::stop_member(
                    tx,
                    member,
                    &origin,
                    &members,
                    &set_digest,
                    budget,
                )
                .await?
                {
                    native_writes.push(write);
                }
                continue;
            }
            crate::tenancy::learning::source::Binding::Capture(binding) => binding,
        };
        let a = binding.association();
        let (connection, state) = a.connection_in(tx, budget).await?;
        let cleanup = state
            .local_cleanup
            .as_ref()
            .context("local capture cleanup absent")?;
        ensure!(
            state.closed && cleanup.origin_json == io::json(&origin, 256)?,
            "local capture owner mismatch"
        );
        let local_cleanup_digest = crate::storage::evolution::digest(&connection.metadata_json)?;
        let row = record(tx, a, JOB, &member.prepared.id, budget)
            .await?
            .context("stopped paid job absent")?;
        a.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        let mut value: Record = serde_json::from_str(&row.body)?;
        ensure!(
            same(&value, &member.prepared)
                && value.version == 3
                && value.execution == member.execution
                && row.kind == record_kind(&value)
                && row.revision == record_revision(&value)
                && row.session_key.as_deref() == Some(value.session.as_str())
                && io::json(&value, RECORD_BYTES)? == row.body,
            "stopped paid identity mismatch"
        );
        validate_live_seed(&value, binding)?;
        let pending_key = a.key(KeyDomain::LearningPending, &[&value.job])?;
        let pending = record(tx, a, PENDING, &pending_key, budget).await?;
        if value.status != Status::Accepted {
            ensure!(
                pending.is_none(),
                "terminal paid job retains pending mirror"
            );
            continue;
        }
        let pending = pending.context("stopped paid pending absent")?;
        a.validate(
            pending.installation_id.as_deref(),
            pending.tenant_id.as_deref(),
            pending.creator_principal_id.as_deref(),
        )?;
        ensure!(
            pending.body == row.body
                && pending.session_key == row.session_key
                && pending.revision == row.revision,
            "stopped paid pending mismatch"
        );
        let recovery = value
            .recovery
            .as_mut()
            .context("stopped paid provenance absent")?;
        let stopped = Stopped {
            members: members.clone(),
            set_digest: set_digest.clone(),
            local_cleanup_digest,
        };
        stopped.validate(&recovery.seed)?;
        if let Some(existing) = &recovery.stopped {
            ensure!(existing == &stopped, "paid stop set replay conflict");
            continue;
        }
        recovery.stopped = Some(stopped);
        ensure!(valid_record(&value), "paid stopped record invalid");
        // Encode once as a bounded preflight before retaining another proposal.
        budget.charge(0, io::json(&value, RECORD_BYTES)?.len())?;
        writes.push((a, row, pending, value));
    }
    for (a, row, pending, value) in writes {
        save(tx, a, &row, &value, budget).await?;
        save(tx, a, &pending, &value, budget).await?;
    }
    for (ns, row, pending, value) in native_writes {
        save(tx, &ns, &row, &value, budget).await?;
        save(tx, &ns, &pending, &value, budget).await?;
    }
    Ok(())
}
