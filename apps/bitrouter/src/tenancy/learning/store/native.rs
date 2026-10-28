//! Native paid jobs use the existing immutable job/counter/mirror schema and H.
pub(in crate::tenancy) mod cleanup;
pub(in crate::tenancy) mod terminal;
use super::*;
use crate::{
    storage::scope::Scope,
    tenancy::{
        execution::session::Binding as NativeBinding,
        experiment::store::completed::{self, Completion},
        learning::{
            native::{Prepare, PreparedSource},
            source,
        },
        session::assessment::store as assessments,
    },
};
use bitrouter_ai::{tenancy::configuration::LearningMode, types::ModelOperation};
use std::sync::Arc;

fn native_source(p: &Prepared) -> Result<&PreparedSource> {
    match &p.source {
        super::PreparedSource::Native(source) => Ok(source),
        super::PreparedSource::Capture(_) => anyhow::bail!("paid native source is not capture"),
    }
}
fn same_source(p: &Prepared, completion: &Completion) -> Result<()> {
    ensure!(
        native_source(p)?.edge.matches(completion.edge()),
        crate::storage::operation_budget::Error::Rejected(ErrorCode::RevisionConflict)
    );
    ensure!(
        completion.edge().unit == p.session
            && completion.edge().checkpoint == p.input.evidence.checkpoint_id
            && completion.packet().prefix_digest == p.input.evidence.prefix_digest,
        "paid native complete source changed"
    );
    Ok(())
}
fn live_seed(value: &Record, binding: &NativeBinding) -> Result<()> {
    let seed = &value
        .recovery
        .as_ref()
        .context("native paid provenance absent")?
        .seed;
    let native = seed.native.as_ref().context("native paid source absent")?;
    ensure!(
        value.version == 4
            && native.descriptor == *binding.descriptor()
            && native.grant == *binding.ownership_grant()
            && seed.origin
                == binding
                    .authority()
                    .host()
                    .service()
                    .startup
                    .as_ref()
                    .context("native paid original owner absent")?
                    .stamp(&binding.authority().host().service().cleanup)?,
        "native paid live owner changed"
    );
    seed.validate()?;
    Ok(())
}
fn decode(row: &records::Model, ns: &namespace::Native) -> Result<Record> {
    ns.validate(
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
    )?;
    let value: Record = serde_json::from_str(&row.body)?;
    ensure!(
        valid_record(&value)
            && value.version == 4
            && row.record_id == value.job
            && row.session_key.as_deref() == Some(value.session.as_str())
            && row.kind == record_kind(&value)
            && row.revision == record_revision(&value)
            && io::json(&value, RECORD_BYTES)? == row.body,
        "native paid record identity changed"
    );
    Ok(value)
}
async fn pair(
    tx: &DatabaseTransaction,
    ns: &namespace::Native,
    value: &Record,
    row: &records::Model,
    budget: &OperationBudget<'_>,
) -> Result<records::Model> {
    let key = ns.key(KeyDomain::LearningPending, &[&value.job])?;
    let pending = record(tx, ns, PENDING, &key, budget)
        .await?
        .context("native paid pending absent")?;
    ns.validate(
        pending.installation_id.as_deref(),
        pending.tenant_id.as_deref(),
        pending.creator_principal_id.as_deref(),
    )?;
    ensure!(
        pending.body == row.body
            && pending.revision == row.revision
            && pending.session_key == row.session_key,
        "native paid mirror changed"
    );
    Ok(pending)
}

pub(in crate::tenancy::learning) async fn prepare(
    tx: &DatabaseTransaction,
    request: &Prepare,
    budget: &OperationBudget<'_>,
) -> Result<Prepared> {
    source::Binding::Native(request.binding.clone())
        .assessment_check()
        .map_err(crate::storage::operation_budget::Error::Rejected)?;
    ensure!(
        tokio::time::Instant::now() < request.deadline,
        crate::storage::operation_budget::Error::Rejected(ErrorCode::CapacityExceeded)
    );
    let completion = completed::live(tx, &request.binding, budget).await?;
    completed::check_retained(&request.retained, completion.profile())?;
    ensure!(
        completion.edge().checkpoint == request.checkpoint,
        crate::storage::operation_budget::Error::Rejected(ErrorCode::RevisionConflict)
    );
    let document = request.retained.document();
    ensure!(
        matches!(
            document.learning.mode,
            LearningMode::Observe | LearningMode::Explore
        ) && document.learning.max_evaluations > 0
            && document.limits.learning_jobs > 0,
        crate::storage::operation_budget::Error::Rejected(ErrorCode::CapabilityMismatch)
    );
    let parent = request
        .retained
        .learning_route(&completion.profile().route)
        .map_err(crate::storage::operation_budget::Error::Rejected)?;
    let reference =
        parent
            .assessment_reference()
            .ok_or(crate::storage::operation_budget::Error::Rejected(
                ErrorCode::CapabilityMismatch,
            ))?;
    let credentials = request
        .retained
        .native_paid_recovery_credentials(
            completion.profile().route.resource_id.as_str(),
            reference,
        )
        .map_err(crate::storage::operation_budget::Error::Rejected)?;
    // This slice cannot acquire an OAuth/count side effect. Qualify exact retained
    // slots under the same H snapshot before logical acceptance or provider work.
    budget.charge(
        credentials.len(),
        credentials
            .len()
            .checked_mul(2 * 1024 * 1024)
            .ok_or(crate::storage::operation_budget::Error::Capacity)?,
    )?;
    let check = || budget.check().map_err(anyhow::Error::from);
    ensure!(
        request
            .binding
            .authority()
            .host()
            .service()
            .resources
            .recovery_static_profile(tx, &credentials, &check)
            .await?,
        crate::storage::operation_budget::Error::Rejected(ErrorCode::CapabilityMismatch)
    );
    let child = request
        .retained
        .select_native(reference.resource_id.as_str(), ModelOperation::Generation)
        .map_err(crate::storage::operation_budget::Error::Rejected)?;
    ensure!(
        child.reference() == reference,
        "native exact evaluator changed"
    );
    child
        .require_capabilities()
        .map_err(crate::storage::operation_budget::Error::Rejected)?;
    let scope = Scope::for_session(request.binding.authority()).0;
    let view = assessments::load(tx, &scope, completion, budget).await?;
    budget.charge(0, io::json_len(view.completion.packet(), 8 * 1024 * 1024)?)?;
    let mut input = assessments::scoring(&view);
    input.expected_revision = request.expected.clone();
    io::json_len(&input, 12 * 1024 * 1024)
        .map_err(|_| crate::storage::operation_budget::Error::Capacity)?;
    let input_digest = judge::input_digest(&input, reference.resource_id.as_str())?;
    let ns = namespace::Native::binding(&request.binding);
    let revision = request.retained.revision_id().clone();
    let id = ns.key(
        KeyDomain::LearningJob,
        &[
            revision.as_str(),
            &view.completion.edge().unit,
            &request.checkpoint,
            &io::json(reference, 4096)?,
            &input_digest,
            judge::NATIVE_JUDGE_VERSION,
        ],
    )?;
    if view.selected.as_ref().map(|r| r.revision_id.as_str()) != request.expected.as_deref() {
        // An exact already accepted command is replayable after its own terminal
        // selected a head. A new logical command must still pass current-head CAS.
        let old = record(tx, &ns, JOB, &id, budget).await?.ok_or(
            crate::storage::operation_budget::Error::Rejected(ErrorCode::RevisionConflict),
        )?;
        let old = decode(&old, &ns)?;
        live_seed(&old, &request.binding)?;
        ensure!(
            old.input_digest == input_digest
                && old.expected_revision == request.expected
                && old.route == *reference
                && old.session == view.completion.edge().unit,
            "native paid replay input changed"
        );
        terminal::source_identity(&old, &view.completion)?;
    }
    Ok(Prepared {
        id,
        session: view.completion.edge().unit.clone(),
        revision,
        route: reference.clone(),
        input,
        input_digest,
        maximum: document.learning.max_evaluations,
        accepted_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        source: super::PreparedSource::Native(Box::new(PreparedSource {
            edge: view.completion.edge().clone(),
            profile: crate::tenancy::learning::Profile {
                route: reference.clone(),
                dependencies: child.dependencies().to_vec(),
                maximum: document.learning.max_evaluations,
                concurrent: u32::from(document.limits.learning_jobs),
            },
        })),
    })
}

pub(in crate::tenancy::learning) async fn accept(
    tx: &DatabaseTransaction,
    binding: &Arc<NativeBinding>,
    p: &Prepared,
    execution: &ExecutionId,
    seed: Option<&super::super::recovery::Seed>,
    budget: &OperationBudget<'_>,
) -> Result<Option<Status>> {
    let ns = namespace::Native::binding(binding);
    if let Some(row) = record(tx, &ns, JOB, &p.id, budget).await? {
        let value = decode(&row, &ns)?;
        ensure!(same(&value, p), "native duplicate logical job changed");
        live_seed(&value, binding)?;
        return Ok(Some(value.status));
    }
    let completion = completed::live(tx, binding, budget).await?;
    same_source(p, &completion)?;
    let scope = Scope::for_session(binding.authority()).0;
    let view = assessments::load(tx, &scope, completion, budget).await?;
    ensure!(
        view.selected.as_ref().map(|r| r.revision_id.as_str())
            == p.input.expected_revision.as_deref(),
        crate::storage::operation_budget::Error::Rejected(ErrorCode::RevisionConflict)
    );
    let owed = reserved(tx, &ns, &p.session, p.checkpoint(), budget).await?;
    assessments::reserve_paid(&view, owed, true, budget)?;
    let seed = seed.context("native durable acceptance needs original owner provenance")?;
    seed.validate()?;
    ensure!(
        seed.matches(p, execution) && seed.native.is_some(),
        "native acceptance provenance changed"
    );
    let key = ns.key(KeyDomain::LearningBudget, &[p.revision.as_str()])?;
    let existing = record(tx, &ns, BUDGET, &key, budget).await?;
    let mut counter = match &existing {
        Some(row) => serde_json::from_str::<Counter>(&row.body)?,
        None => Counter {
            version: 1,
            configuration: p.revision.clone(),
            accepted: 0,
        },
    };
    ensure!(
        counter.version == 1
            && counter.configuration == p.revision
            && counter.accepted <= p.maximum
            && existing.as_ref().is_none_or(|row| row.session_key.is_none()
                && row.revision.checked_add(1) == Some(i64::from(counter.accepted))),
        "native immutable revision budget changed"
    );
    ensure!(
        counter.accepted < p.maximum,
        crate::storage::operation_budget::Error::Rejected(ErrorCode::CapacityExceeded)
    );
    counter.accepted = counter
        .accepted
        .checked_add(1)
        .context("native logical budget overflow")?;
    // This is the bounded, cited projection already produced by CompletedRun,
    // not a raw journal or a replacement authority handle.
    budget.charge(
        0,
        io::json_len(&p.input.evidence, RECORD_BYTES)
            .map_err(|_| crate::storage::operation_budget::Error::Capacity)?,
    )?;
    let value = Record {
        version: 4,
        native_evidence: Some(p.input.evidence.clone()),
        job: p.id.clone(),
        session: p.session.clone(),
        configuration: p.revision.clone(),
        route: p.route.clone(),
        checkpoint: p.checkpoint().into(),
        prefix_digest: p.input.evidence.prefix_digest.clone(),
        input_digest: p.input_digest.clone(),
        expected_revision: p.input.expected_revision.clone(),
        execution: execution.clone(),
        status: Status::Accepted,
        accepted_at: p.accepted_at.clone(),
        output: None,
        assessment: None,
        history_reservation: Some(HistoryReservation {
            checkpoint: p.checkpoint().into(),
            rows: 1,
            bytes: history::REVISION_BYTES,
            state: ReservationState::Held,
        }),
        recovery: Some(super::super::recovery::Retained {
            seed: seed.clone(),
            observed: None,
            stopped: None,
            native_cleanup: None,
        }),
    };
    ensure!(valid_record(&value), "native accepted record invalid");
    // Reserve the 512 KiB parsed answer plus 64 KiB for its observed hashes,
    // exact <=8-member stop-set, descriptor/grant cleanup ACK and terminal IDs.
    // Seed itself remains <=16 KiB, each identifier <=256; no new row cap.
    ensure!(
        io::json_len(&value, RECORD_BYTES)
            .map_err(|_| crate::storage::operation_budget::Error::Capacity)?
            .checked_add(512 * 1024 + 64 * 1024)
            .is_some_and(|bytes| bytes <= RECORD_BYTES),
        crate::storage::operation_budget::Error::Capacity
    );
    // Encode all fixed proposals and reserve writes before counter/job DML.
    budget.charge(
        3,
        io::json_len(&value, RECORD_BYTES)?
            .checked_mul(2)
            .and_then(|n| n.checked_add(8192))
            .context("native acceptance write bound")?,
    )?;
    assessments::retain_source(
        tx,
        &scope,
        &Scope::for_session(binding.authority()).1,
        &view.completion,
        budget,
    )
    .await?;
    match &existing {
        Some(row) => save(tx, &ns, row, &counter, budget).await?,
        None => insert(tx, &ns, &key, BUDGET, None, &counter, budget).await?,
    }
    insert(tx, &ns, &p.id, ACTIVE, Some(&p.session), &value, budget).await?;
    let pending = ns.key(KeyDomain::LearningPending, &[&p.id])?;
    insert(tx, &ns, &pending, PENDING, Some(&p.session), &value, budget).await?;
    Ok(None)
}

pub(in crate::tenancy) async fn reserved_for(
    tx: &DatabaseTransaction,
    scope: Scope,
    provenance: crate::storage::scope::Provenance,
    session: &str,
    checkpoint: &str,
    budget: &OperationBudget<'_>,
) -> Result<history::Allowance> {
    reserved(
        tx,
        &namespace::Native::new(scope, provenance),
        session,
        checkpoint,
        budget,
    )
    .await
}
pub(in crate::tenancy::learning::store) async fn reserved(
    tx: &DatabaseTransaction,
    ns: &namespace::Native,
    session: &str,
    checkpoint: &str,
    budget: &OperationBudget<'_>,
) -> Result<history::Allowance> {
    let rows = io::rows(
        tx,
        records::Entity::find()
            .filter(scope(ns))
            .filter(records::Column::ScopeId.eq(ns.key(KeyDomain::LearningPending, &[])?))
            .filter(records::Column::Kind.eq(PENDING))
            .filter(records::Column::SessionKey.eq(session))
            .lock_exclusive(),
        8,
        8 * (RECORD_BYTES + 4096),
        budget,
    )
    .await?;
    let mut total = history::Allowance::default();
    for pending in rows {
        ns.validate(
            pending.installation_id.as_deref(),
            pending.tenant_id.as_deref(),
            pending.creator_principal_id.as_deref(),
        )?;
        let value: Record = serde_json::from_str(&pending.body)?;
        let row = record(tx, ns, JOB, &value.job, budget)
            .await?
            .context("native reserved job absent")?;
        let checked = decode(&row, ns)?;
        ensure!(
            checked.status == Status::Accepted
                && checked.session == session
                && pending.record_id == ns.key(KeyDomain::LearningPending, &[&checked.job])?
                && pending.body == row.body
                && pending.revision == row.revision
                && pending.session_key == row.session_key,
            "native paid reservation mirror changed"
        );
        if checked.checkpoint == checkpoint {
            total.rows = total
                .rows
                .checked_add(1)
                .context("native reservation row overflow")?;
            total.bytes = total
                .bytes
                .checked_add(history::REVISION_BYTES)
                .context("native reservation bytes overflow")?;
        }
    }
    Ok(total)
}
