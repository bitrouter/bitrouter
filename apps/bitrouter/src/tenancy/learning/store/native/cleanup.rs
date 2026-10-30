//! Actual local close ACK and exact Registry stop-set preservation, never a DTO proof.
use super::*;
use crate::tenancy::{
    learning::{
        native::Cleanup,
        recovery::{Member, Stopped},
    },
    origin::Stamp,
};

async fn pending_for(
    tx: &DatabaseTransaction,
    binding: &NativeBinding,
    budget: &OperationBudget<'_>,
) -> Result<Vec<(namespace::Native, records::Model, records::Model, Record)>> {
    let ns = namespace::Native::binding(binding);
    let rows = io::rows(
        tx,
        records::Entity::find()
            .filter(scope(&ns))
            .filter(records::Column::ScopeId.eq(ns.key(KeyDomain::LearningPending, &[])?))
            .filter(records::Column::Kind.eq(PENDING))
            .filter(records::Column::CreatorPrincipalId.eq(binding.descriptor().principal.as_str()))
            .lock_exclusive(),
        8,
        8 * (RECORD_BYTES + 4096),
        budget,
    )
    .await?;
    let mut result = Vec::new();
    for pending in rows {
        let value: Record = serde_json::from_str(&pending.body)?;
        ensure!(valid_record(&value), "invalid local cleanup pending record");
        let Some(native) = value.recovery.as_ref().and_then(|r| r.seed.native.as_ref()) else {
            continue;
        };
        if native.descriptor.name != binding.descriptor().name {
            continue;
        }
        ensure!(
            native.descriptor == *binding.descriptor()
                && native.grant == *binding.ownership_grant(),
            "native cleanup owner epoch changed"
        );
        let ns = namespace::Native::binding(binding);
        let row = record(tx, &ns, JOB, &value.job, budget)
            .await?
            .context("native cleanup job absent")?;
        let checked = decode(&row, &ns)?;
        live_seed(&checked, binding)?;
        ensure!(
            checked.status == Status::Accepted
                && pending.body == row.body
                && pending.revision == row.revision
                && pending.session_key == row.session_key
                && pending.record_id == ns.key(KeyDomain::LearningPending, &[&checked.job])?,
            "native cleanup mirror changed"
        );
        result.push((ns, row, pending, checked));
    }
    Ok(result)
}
async fn observed(
    tx: &DatabaseTransaction,
    binding: &NativeBinding,
    value: &Record,
    budget: &OperationBudget<'_>,
) -> Result<Cleanup> {
    let native = value
        .recovery
        .as_ref()
        .and_then(|r| r.seed.native.as_ref())
        .context("native cleanup source absent")?;
    let completion = completed::closed(tx, binding, &native.source.run, budget).await?;
    ensure!(
        native.source.matches(completion.edge()),
        "native cleanup complete source changed"
    );
    let loaded = crate::managed_store::owned::load_for_model_in(tx, binding.authority(), budget)
        .await?
        .context("native closed journal absent")?;
    let digest = crate::storage::evolution::digest(&loaded.journal)?;
    Ok(Cleanup {
        origin: binding
            .authority()
            .host()
            .service()
            .startup
            .as_ref()
            .context("native original owner absent")?
            .stamp(&binding.authority().host().service().cleanup)?,
        descriptor: binding.descriptor().clone(),
        grant: binding.ownership_grant().clone(),
        head: completion.edge().head.clone(),
        journal_digest: digest,
    })
}

pub(in crate::tenancy::learning) async fn stopped(
    tx: &DatabaseTransaction,
    proof: &crate::harness::resources::managed::session::owned::CleanupProof,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let binding = proof.binding();
    let mut writes = pending_for(tx, binding, budget).await?;
    for (_, _, _, value) in &mut writes {
        let cleanup = observed(tx, binding, value, budget).await?;
        let recovery = value
            .recovery
            .as_mut()
            .context("native cleanup provenance absent")?;
        if let Some(previous) = &recovery.native_cleanup {
            ensure!(previous == &cleanup, "native cleanup ACK replay changed");
        } else {
            recovery.native_cleanup = Some(cleanup);
        }
        ensure!(valid_record(value), "native cleanup result invalid");
        budget.charge(
            2,
            io::json_len(value, RECORD_BYTES)?
                .checked_mul(2)
                .context("native cleanup write bound")?,
        )?;
    }
    for (ns, row, pending, value) in writes {
        if io::json(&value, RECORD_BYTES)? != row.body {
            save(tx, &ns, &row, &value, budget).await?;
            save(tx, &ns, &pending, &value, budget).await?;
        }
    }
    Ok(())
}

pub(in crate::tenancy::learning::store) async fn stop_member(
    tx: &DatabaseTransaction,
    owner: &Arc<crate::tenancy::learning::owner::Owner>,
    origin: &Stamp,
    members: &[Member],
    set_digest: &str,
    budget: &OperationBudget<'_>,
) -> Result<Option<(namespace::Native, records::Model, records::Model, Record)>> {
    let source::Binding::Native(binding) = &owner.binding else {
        anyhow::bail!("native stop-set member source changed");
    };
    let ns = namespace::Native::binding(binding);
    let row = record(tx, &ns, JOB, &owner.prepared.id, budget)
        .await?
        .context("stopped native paid job absent")?;
    let mut value = decode(&row, &ns)?;
    live_seed(&value, binding)?;
    ensure!(
        same(&value, &owner.prepared) && value.execution == owner.execution,
        "native paid stop-set identity changed"
    );
    let pending_key = ns.key(KeyDomain::LearningPending, &[&value.job])?;
    if value.status != Status::Accepted {
        ensure!(
            record(tx, &ns, PENDING, &pending_key, budget)
                .await?
                .is_none(),
            "terminal native paid mirror survived"
        );
        return Ok(None);
    }
    let pending = pair(tx, &ns, &value, &row, budget).await?;
    let actual = observed(tx, binding, &value, budget).await?;
    ensure!(
        &actual.origin == origin,
        "native stopped acquisition changed"
    );
    let recovery = value
        .recovery
        .as_mut()
        .context("native stopped provenance absent")?;
    ensure!(
        recovery.native_cleanup.as_ref() == Some(&actual),
        "native local cleanup lacks actual host ACK"
    );
    let stopped = Stopped {
        members: members.to_vec(),
        set_digest: set_digest.into(),
        local_cleanup_digest: crate::storage::evolution::digest(&actual)?,
    };
    stopped.validate(&recovery.seed)?;
    if let Some(previous) = &recovery.stopped {
        ensure!(previous == &stopped, "native stop-set replay changed");
        return Ok(None);
    }
    recovery.stopped = Some(stopped);
    ensure!(valid_record(&value), "native stop-set result invalid");
    budget.charge(0, io::json_len(&value, RECORD_BYTES)?)?;
    Ok(Some((ns, row, pending, value)))
}

/// A released journal does not permit reopening over an outstanding accepted
/// job. This runs on the real H Load path before a new native epoch is issued.
pub(in crate::tenancy) async fn guard_open(
    tx: &DatabaseTransaction,
    authority: &crate::tenancy::execution::session::Authority,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let (scope, _) = Scope::for_session(authority);
    let rows = io::rows(
        tx,
        records::Entity::find()
            .filter(scope.predicate(records::Column::InstallationId, records::Column::TenantId))
            .filter(records::Column::Kind.eq(PENDING))
            .filter(
                records::Column::CreatorPrincipalId.eq(authority.descriptor().principal.as_str()),
            )
            .lock_exclusive(),
        8,
        8 * (RECORD_BYTES + 4096),
        budget,
    )
    .await?;
    for row in rows {
        let value: Record = serde_json::from_str(&row.body)?;
        ensure!(valid_record(&value), "native open paid inventory invalid");
        if let Some(native) = value.recovery.as_ref().and_then(|r| r.seed.native.as_ref()) {
            ensure!(
                native.descriptor.name != authority.descriptor().name,
                crate::storage::operation_budget::Error::Rejected(ErrorCode::RevisionConflict)
            );
        }
    }
    Ok(())
}
