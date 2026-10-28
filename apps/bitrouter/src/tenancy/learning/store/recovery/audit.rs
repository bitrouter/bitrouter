use super::*;
use crate::{
    storage::scope::Scope,
    tenancy::learning::recovery::{Member, RecoveryIdentity},
};
use bitrouter_ai::tenancy::identity::PrincipalId;
use sea_orm::{Condition, QueryOrder};
use std::collections::BTreeSet;

const MAX_JOBS: usize = 128;
const MAX_COUNTERS: usize = 128;
const MAX_PENDING: usize = 8;

async fn inventory(
    tx: &DatabaseTransaction,
    state: &Snapshot,
    budget: &OperationBudget<'_>,
) -> Result<Vec<records::Model>> {
    ensure!(state.tenants.len() <= 256, "paid tenant inventory bound");
    let partitions = state
        .tenants
        .keys()
        .map(|tenant| crate::storage::partition::learning_pending(&state.installation, tenant))
        .collect::<Result<Vec<_>>>()?;
    // Include namespace occupants even if their redundant partition or tenant
    // metadata was damaged. Legacy ALL-NULL rows outside these keys are untouched.
    let selected = Condition::any()
        .add(records::Column::InstallationId.eq(state.installation.as_str()))
        .add(records::Column::ScopeId.is_in(partitions));
    io::rows(
        tx,
        records::Entity::find()
            .filter(selected)
            .filter(records::Column::Kind.is_in([JOB, ACTIVE, BUDGET, PENDING]))
            .order_by_asc(records::Column::RecordId)
            .lock_exclusive(),
        MAX_JOBS + MAX_COUNTERS + MAX_PENDING,
        16 * 1024 * 1024,
        budget,
    )
    .await
}

pub(super) async fn inspect(
    tx: &DatabaseTransaction,
    access: &Access<'_>,
    at: &Timestamp,
    budget: &OperationBudget<'_>,
    prior: Option<&BTreeMap<(TenantId, RevisionId), Arc<closure::Configuration>>>,
) -> Result<Plan> {
    let Access {
        database,
        state,
        origin,
        proof,
        resources,
    } = *access;
    origin.bootstrap_stamp(proof)?;
    let rows = inventory(tx, state, budget).await?;
    let mut counters = BTreeMap::new();
    let mut pending = BTreeMap::new();
    let mut jobs = Vec::new();
    let mut executions = BTreeSet::new();
    for row in &rows {
        budget.charge(0, row.body.len())?;
        let tenant: TenantId = row
            .tenant_id
            .clone()
            .context("paid row tenant absent")?
            .try_into()?;
        let principal: PrincipalId = row
            .creator_principal_id
            .clone()
            .context("paid row creator absent")?
            .try_into()?;
        ensure!(
            row.installation_id.as_deref() == Some(state.installation.as_str())
                && state.tenants.contains_key(&tenant)
                && state.principals.contains_key(&principal)
                && row.scope_id
                    == crate::storage::partition::learning_pending(&state.installation, &tenant)?
                && row.revision >= 0
                && row.body.len() <= RECORD_BYTES,
            "paid row namespace or metadata invalid"
        );
        let identity = RecoveryIdentity::checked(
            database,
            origin,
            proof,
            &state.installation,
            &tenant,
            principal,
        )?;
        let (scope, provenance) = Scope::for_paid_recovery(&identity);
        scope.validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        match row.kind.as_str() {
            BUDGET => {
                let value: Counter = serde_json::from_str(&row.body)?;
                ensure!(
                    value.version == 1
                        && value.accepted > 0
                        && row.session_key.is_none()
                        && row.revision.checked_add(1) == Some(i64::from(value.accepted))
                        && row.record_id
                            == scope.physical_key(
                                KeyDomain::LearningBudget,
                                "",
                                &[value.configuration.as_str()]
                            )?
                        && io::json(&value, RECORD_BYTES)? == row.body,
                    "paid counter identity invalid"
                );
                ensure!(
                    counters.len() < MAX_COUNTERS
                        && counters
                            .insert((tenant, value.configuration.clone()), value)
                            .is_none(),
                    "paid counter capacity or duplicate"
                );
            }
            PENDING => {
                ensure!(
                    pending.len() < MAX_PENDING
                        && pending.insert(row.record_id.clone(), row).is_none(),
                    "paid pending inventory bound"
                );
            }
            JOB | ACTIVE => {
                let value: Record = serde_json::from_str(&row.body)?;
                ensure!(
                    valid_record(&value)
                        && row.kind == record_kind(&value)
                        && row.record_id == value.job
                        && row.session_key.as_deref() == Some(value.session.as_str())
                        && row.revision == record_revision(&value)
                        && io::json(&value, RECORD_BYTES)? == row.body
                        && executions.insert(value.execution.clone()),
                    "paid job identity invalid"
                );
                ensure!(jobs.len() < MAX_JOBS, "paid lifetime job audit bound");
                jobs.push((row, value, scope, provenance, identity));
            }
            _ => anyhow::bail!("paid record kind changed"),
        }
    }
    let mut sources = Vec::new();
    let mut configurations = BTreeMap::new();
    let mut totals = BTreeMap::<(TenantId, RevisionId), u32>::new();
    let mut counts = BTreeMap::<TenantId, usize>::new();
    let mut members = BTreeMap::new();
    let mut sets = BTreeMap::new();
    let mut writes = Vec::new();
    for (row, value, scope, provenance, identity) in &jobs {
        let tenant: TenantId = row
            .tenant_id
            .clone()
            .context("paid tenant absent")?
            .try_into()?;
        let key = (tenant.clone(), value.configuration.clone());
        if !configurations.contains_key(&key) {
            let profile = closure::configuration(
                tx,
                &state.installation,
                &tenant,
                &value.configuration,
                resources,
                budget,
                prior.and_then(|profiles| profiles.get(&key)),
            )
            .await?;
            sources.push(profile.digest.clone());
            configurations.insert(key.clone(), profile);
        }
        let profile = configurations.get(&key).context("paid profile absent")?;
        let total = totals.entry(key).or_default();
        *total = total
            .checked_add(1)
            .context("paid accepted count overflow")?;
        ensure!(
            *total <= profile.maximum,
            "paid retained revision budget exceeded"
        );
        let closed = closure::job(
            tx,
            row,
            value,
            (scope.clone(), provenance.clone()),
            profile,
            identity,
            budget,
        )
        .await?;
        sources.push(closed.digest.clone());
        let a = &closed.association;
        let pending_key = a.key(KeyDomain::LearningPending, &[&value.job])?;
        let mirror = pending.remove(&pending_key);
        if let Some(retained) = &value.recovery {
            let member = Member::from_seed(&retained.seed)?;
            ensure!(
                members
                    .insert(member.execution.clone(), (member, &retained.seed.origin))
                    .is_none(),
                "duplicate paid origin member"
            );
        }
        if value.status != Status::Accepted {
            ensure!(mirror.is_none(), "terminal paid job retains pending mirror");
            sources.push(closure::terminal(tx, a, value, &closed.input, budget).await?);
            continue;
        }
        let count = counts.entry(tenant).or_default();
        *count += 1;
        ensure!(
            *count <= 2 && writes.len() < MAX_PENDING,
            "paid recovery pending capacity"
        );
        let mirror = mirror.context("accepted paid mirror absent")?;
        ensure!(
            mirror.body == row.body
                && mirror.session_key == row.session_key
                && mirror.revision == row.revision
                && mirror.installation_id == row.installation_id
                && mirror.tenant_id == row.tenant_id
                && mirror.creator_principal_id == row.creator_principal_id,
            "paid pending mirror changed"
        );
        let retained = value
            .recovery
            .as_ref()
            .context("unsupported old paid acceptance")?;
        let seed = &retained.seed;
        match a {
            namespace::Stored::Capture(a) => ensure!(
                seed.descriptor.as_ref() == Some(a.descriptor())
                    && seed.connection.as_deref() == Some(a.connection()),
                "paid seed differs from historical capture"
            ),
            namespace::Stored::Native(_) => {
                ensure!(seed.native.is_some(), "native paid seed absent")
            }
        }
        origin.bootstrap_excludes(proof, &seed.origin)?;
        let stopped = retained
            .stopped
            .as_ref()
            .context("actual paid shutdown proof absent")?;
        stopped.validate(seed)?;
        let cleanup_digest = match a {
            namespace::Stored::Capture(a) => {
                let (connection, capture) = a.connection_in(tx, budget).await?;
                let cleanup = capture
                    .local_cleanup
                    .as_ref()
                    .context("paid local cleanup ACK absent")?;
                ensure!(
                    capture.closed && cleanup.origin_json == io::json(&seed.origin, 256)?,
                    "paid local cleanup owner changed"
                );
                crate::storage::evolution::digest(&connection.metadata_json)?
            }
            namespace::Stored::Native(_) => native::cleanup(tx, identity, retained, budget).await?,
        };
        ensure!(
            cleanup_digest == stopped.local_cleanup_digest,
            "paid local cleanup digest changed"
        );
        sources.push(cleanup_digest);
        match sets.entry(stopped.set_digest.clone()) {
            std::collections::btree_map::Entry::Vacant(entry) => {
                entry.insert((seed.origin.clone(), stopped.members.clone()));
            }
            std::collections::btree_map::Entry::Occupied(entry) => {
                ensure!(
                    entry.get() == &(seed.origin.clone(), stopped.members.clone()),
                    "paid stop set conflict"
                );
            }
        }
        let refs = if seed.native.is_some() {
            profile
                .execution
                .native_paid_recovery_credentials(&closed.subject, &seed.evaluator)
        } else {
            profile.execution.paid_recovery_credentials(
                &seed
                    .descriptor
                    .as_ref()
                    .context("capture seed absent")?
                    .model,
                &seed.evaluator,
            )
        }
        .map_err(|_| anyhow::anyhow!("paid retained execution is outside recovery profile"))?;
        for reference in refs {
            resources
                .paid_recovery_static(tx, &reference, budget)
                .await?;
        }
        let saved = closure::receipt(tx, seed, budget).await?;
        if let Some(observed) = &retained.observed {
            let candidate = saved
                .as_ref()
                .context("observed paid answer has no durable receipt")?;
            ensure!(
                candidate.digest == observed.receipt_digest,
                "paid observed receipt changed"
            );
        }
        let missing_receipt = saved.is_none();
        let candidate = saved.map(Ok).unwrap_or_else(|| seed.unknown(at))?;
        sources.push(candidate.digest.clone());
        budget.charge(
            0,
            row.body
                .len()
                .checked_mul(2)
                .context("paid proposal size")?,
        )?;
        let mut terminal: Record = serde_json::from_str(&row.body)?;
        let assessment = write::assessment(a, &terminal, &closed.input, at, budget)?;
        if let Some(revision) = &assessment {
            terminal.status = Status::Unselected;
            terminal.assessment = Some(revision.revision_id.clone());
            terminal
                .history_reservation
                .as_mut()
                .context("paid reservation absent")?
                .state = ReservationState::Consumed {
                actual_bytes: io::json(revision, RECORD_BYTES)?.len(),
            };
        } else {
            terminal.status = Status::Interrupted;
            terminal
                .history_reservation
                .as_mut()
                .context("paid reservation absent")?
                .state = ReservationState::ReleasedWithoutRevision;
        }
        ensure!(valid_record(&terminal), "paid repaired record invalid");
        writes.push(Write {
            association: closed.association,
            row: (*row).clone(),
            pending: mirror.clone(),
            terminal,
            assessment,
            receipt: Arc::new(candidate),
            missing_receipt,
        });
    }
    ensure!(pending.is_empty(), "orphan paid pending index");
    ensure!(
        counters.len() == totals.len(),
        "paid counter/job coverage differs"
    );
    for (key, total) in totals {
        ensure!(
            counters
                .get(&key)
                .is_some_and(|counter| counter.accepted == total),
            "paid cumulative counter differs from durable jobs"
        );
    }
    for (origin, set) in sets.values() {
        for expected in set {
            let (actual, actual_origin) = members
                .get(&expected.execution)
                .context("paid stopped member absent")?;
            ensure!(
                actual == expected && *actual_origin == origin,
                "paid stopped member changed"
            );
        }
        for write in &writes {
            let seed = &write
                .terminal
                .recovery
                .as_ref()
                .context("paid provenance absent")?
                .seed;
            if &seed.origin == origin {
                ensure!(
                    set.contains(&Member::from_seed(seed)?),
                    "paid stop set omits unresolved sibling"
                );
            }
        }
    }
    budget.check()?;
    Ok(Plan {
        rows,
        configurations,
        writes,
        sources,
        at: at.clone(),
    })
}
