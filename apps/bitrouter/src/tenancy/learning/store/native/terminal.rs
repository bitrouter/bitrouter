//! Actual paid observations precede immutable native revisions; selection is a CAS.
use super::*;
use crate::{
    storage::scope::MetadataRef,
    tenancy::learning::{native::Seed, owner::Observation},
};

pub(in crate::tenancy::learning) async fn observe(
    tx: &DatabaseTransaction,
    binding: &Arc<NativeBinding>,
    p: &Prepared,
    execution: &ExecutionId,
    observation: &Observation,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let ns = namespace::Native::binding(binding);
    let row = record(tx, &ns, JOB, &p.id, budget)
        .await?
        .context("native observed job absent")?;
    let mut value = decode(&row, &ns)?;
    live_seed(&value, binding)?;
    ensure!(
        same(&value, p) && value.execution == *execution && value.status == Status::Accepted,
        "native paid observation identity changed"
    );
    let pending = pair(tx, &ns, &value, &row, budget).await?;
    let output = &observation.output;
    io::json_len(output, 512 * 1024)?;
    output_identity(&value, output)?;
    output.evaluation.aggregate(&p.input.evidence)?;
    let recovery = value
        .recovery
        .as_mut()
        .context("native observed provenance absent")?;
    recovery.seed.receipt(&observation.terminal)?;
    let digest = observation.terminal.learning_digest();
    verify_receipt(tx, &recovery.seed, digest, budget).await?;
    let output_digest = crate::storage::evolution::digest(output)?;
    if let Some(saved) = &recovery.observed {
        ensure!(
            saved.receipt_digest == digest
                && saved.output_digest == output_digest
                && io::json(&value.output, RECORD_BYTES)? == io::json(&Some(output), RECORD_BYTES)?,
            "native observed output replay changed"
        );
        return Ok(());
    }
    recovery.observed = Some(super::super::super::recovery::Observed {
        receipt_digest: digest.to_owned(),
        output_digest,
    });
    value.output = Some(output.clone());
    ensure!(valid_record(&value), "native observed record invalid");
    save(tx, &ns, &row, &value, budget).await?;
    save(tx, &ns, &pending, &value, budget).await?;
    Ok(())
}
fn output_identity(value: &Record, output: &judge::JudgeOutput) -> Result<()> {
    ensure!(
        output.request_id == value.execution.as_str()
            && output.model == value.route.resource_id.as_str()
            && output.judge_version == judge::NATIVE_JUDGE_VERSION
            && output.input_digest == value.input_digest,
        "native judge output identity changed"
    );
    Ok(())
}
pub(in crate::tenancy::learning::store) fn revision_input(
    value: &Record,
    evidence: &crate::core::evolution::evidence::EvidencePacket,
) -> Result<RevisionInput> {
    let output = value.output.as_ref().context("native paid answer absent")?;
    output_identity(value, output)?;
    let evaluator = io::json(&value.route, 4096)?;
    let contract = crate::core::evolution::scoring::measurement_contract_for(
        &evidence.projection_version,
        AssessmentSource::Agentic,
        &evaluator,
        judge::NATIVE_JUDGE_VERSION,
    )?;
    Ok(RevisionInput {
        submission_id: value.job.clone(),
        checkpoint_id: value.checkpoint.clone(),
        expected_revision: value.expected_revision.clone(),
        source: AssessmentSource::Agentic,
        evaluator_id: evaluator,
        evaluator_version: judge::NATIVE_JUDGE_VERSION.into(),
        assessment: Some(output.evaluation.assessment(evidence, &contract)?),
        reason: output.evaluation.summary.clone(),
    })
}
fn seed(value: &Record) -> Result<&Seed> {
    value
        .recovery
        .as_ref()
        .and_then(|r| r.seed.native.as_ref())
        .context("native source seed absent")
}
pub(in crate::tenancy::learning::store) fn source_identity(
    value: &Record,
    completion: &Completion,
) -> Result<()> {
    let source = seed(value)?;
    ensure!(
        completion.matches_source(&source.source)
            && value.session == completion.edge().unit
            && value.checkpoint == completion.edge().checkpoint
            && value.prefix_digest == completion.packet().prefix_digest,
        "native paid assessment source differs from actual run"
    );
    ensure!(
        io::json(
            value
                .native_evidence
                .as_ref()
                .context("native accepted evidence absent")?,
            RECORD_BYTES
        )? == io::json(completion.packet(), RECORD_BYTES)?,
        "native paid retained projection changed"
    );
    let input = ScoringInput {
        rubric_version: rubric::RUBRIC_VERSION,
        library: rubric::library(),
        expected_revision: value.expected_revision.clone(),
        evidence: completion.packet().clone(),
    };
    ensure!(
        judge::input_digest(&input, value.route.resource_id.as_str())? == value.input_digest,
        "native paid assessment input changed"
    );
    Ok(())
}

pub(in crate::tenancy::learning) async fn finish(
    tx: &DatabaseTransaction,
    binding: &Arc<NativeBinding>,
    p: &Prepared,
    execution: &ExecutionId,
    output: Option<&judge::JudgeOutput>,
    select: bool,
    budget: &OperationBudget<'_>,
) -> Result<Status> {
    let ns = namespace::Native::binding(binding);
    let row = record(tx, &ns, JOB, &p.id, budget)
        .await?
        .context("native accepted job absent")?;
    let mut value = decode(&row, &ns)?;
    live_seed(&value, binding)?;
    ensure!(
        same(&value, p)
            && value.execution == *execution
            && value.expected_revision == p.input.expected_revision
            && value.accepted_at == p.accepted_at,
        "native paid terminal identity changed"
    );
    ensure!(
        io::json(&value.output, RECORD_BYTES)? == io::json(&output, RECORD_BYTES)?,
        "native paid terminal differs from acknowledged observation"
    );
    if value.status != Status::Accepted {
        return Ok(value.status);
    }
    let pending = pair(tx, &ns, &value, &row, budget).await?;
    let mut reservation = ReservationState::ReleasedWithoutRevision;
    value.status = Status::Failed;
    if let Some(output) = output {
        output_identity(&value, output)?;
        let observed = value
            .recovery
            .as_ref()
            .and_then(|r| r.observed.as_ref())
            .context("native paid output has no actual receipt/observation ACK")?;
        verify_receipt(
            tx,
            &value.recovery.as_ref().context("native recovery")?.seed,
            &observed.receipt_digest,
            budget,
        )
        .await?;
        // The already admitted owner may retain an answer after revocation.
        // This reader checks original origin/source only; selection separately
        // requires the actor's current authority check and unchanged head.
        let completion = completed::finalizing(tx, binding, budget).await?;
        same_source(p, &completion)?;
        let (scope, provenance) = Scope::for_session(binding.authority());
        let view = assessments::load(tx, &scope, completion, budget).await?;
        let current = view.selected.as_ref().map(|r| r.revision_id.clone());
        let selected = select
            && current == value.expected_revision
            && view
                .selected
                .as_ref()
                .is_none_or(|r| r.input.source != AssessmentSource::Human);
        let reason = if !select {
            "authority_changed"
        } else if current != value.expected_revision {
            "head_changed"
        } else if !selected {
            "human_revision_preserved"
        } else {
            "selected"
        };
        let input = revision_input(&value, view.completion.packet())?;
        let id = ns.key(KeyDomain::AssessmentRevision, &[&p.session, &p.id])?;
        let revision = AssessmentRevision {
            revision_id: id.clone(),
            input_digest: crate::storage::evolution::digest(&input)?,
            input,
            supersedes: if selected { current.clone() } else { None },
            selected_on_submission: selected,
            selection_reason: reason.into(),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        let body = io::json(&revision, history::REVISION_BYTES)?;
        // Acceptance reserved this maximal terminal; finishing consumes it,
        // never re-admits a logical job or refunds the revision counter.
        budget.charge(
            2,
            body.len()
                .checked_add(8192)
                .context("native terminal write bound")?,
        )?;
        assessments::retain_source(tx, &scope, &provenance, &view.completion, budget).await?;
        let meta = ns.metadata()?;
        ensure!(
            revisions::Entity::insert(revisions::ActiveModel {
                revision_id: Set(id.clone()),
                session_key: Set(p.session.clone()),
                checkpoint_id: Set(value.checkpoint.clone()),
                created_at: Set(revision.created_at),
                revision_json: Set(body.clone()),
                installation_id: Set(meta.installation.clone()),
                tenant_id: Set(meta.tenant.clone()),
                creator_principal_id: Set(meta.creator.clone()),
            })
            .exec_without_returning(tx)
            .await?
                == 1,
            "native paid revision conflict"
        );
        budget.check()?;
        if selected {
            if let Some(current) = current {
                ensure!(
                    heads::Entity::update_many()
                        .filter(
                            scope.predicate(heads::Column::InstallationId, heads::Column::TenantId)
                        )
                        .filter(heads::Column::SessionKey.eq(&p.session))
                        .filter(heads::Column::RevisionId.eq(current))
                        .col_expr(heads::Column::RevisionId, Expr::value(Some(id.clone())))
                        .exec(tx)
                        .await?
                        .rows_affected
                        == 1,
                    "native paid head CAS failed"
                );
            } else {
                ensure!(
                    heads::Entity::insert(heads::ActiveModel {
                        session_key: Set(p.session.clone()),
                        revision_id: Set(Some(id.clone())),
                        installation_id: Set(meta.installation),
                        tenant_id: Set(meta.tenant),
                        creator_principal_id: Set(meta.creator),
                    })
                    .exec_without_returning(tx)
                    .await?
                        == 1,
                    "native paid absent head conflict"
                );
            }
            budget.check()?;
        }
        reservation = ReservationState::Consumed {
            actual_bytes: body.len(),
        };
        value.assessment = Some(id);
        value.status = if selected {
            Status::Completed
        } else {
            Status::Unselected
        };
    }
    value
        .history_reservation
        .as_mut()
        .context("native terminal reservation absent")?
        .state = reservation;
    ensure!(valid_record(&value), "native terminal record invalid");
    save_as(tx, &ns, &row, JOB, &value, budget).await?;
    budget.charge(1, 4096)?;
    ensure!(
        records::Entity::delete_many()
            .filter(scope(&ns))
            .filter(records::Column::RecordId.eq(pending.record_id))
            .filter(records::Column::Kind.eq(PENDING))
            .filter(records::Column::Revision.eq(pending.revision))
            .filter(records::Column::Body.eq(pending.body))
            .exec(tx)
            .await?
            .rows_affected
            == 1,
        "native paid pending CAS failed"
    );
    budget.check()?;
    Ok(value.status)
}

/// A native Agentic history row must name the exact durable paid result and035
/// terminal. Equality of labels, scores or evaluator model names is insufficient.
pub(in crate::tenancy) async fn validate_revision(
    tx: &DatabaseTransaction,
    scope: &Scope,
    completion: &Completion,
    revision: &AssessmentRevision,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let row = io::one(
        tx,
        records::Entity::find_by_id(&revision.input.submission_id)
            .filter(scope.predicate(records::Column::InstallationId, records::Column::TenantId))
            .filter(records::Column::Kind.eq(JOB)),
        RECORD_BYTES + 4096,
        budget,
    )
    .await?
    .context("native paid assessment job absent")?;
    let provenance = scope.validate_metadata(MetadataRef {
        installation: row.installation_id.as_deref(),
        tenant: row.tenant_id.as_deref(),
        creator: row.creator_principal_id.as_deref(),
    })?;
    ensure!(
        row.creator_principal_id.as_deref() == Some(completion.principal()),
        "native paid creator changed"
    );
    let ns = namespace::Native::new(scope.clone(), provenance);
    let value = decode(&row, &ns)?;
    ensure!(
        matches!(value.status, Status::Completed | Status::Unselected)
            && value.assessment.as_deref() == Some(revision.revision_id.as_str())
            && revision.revision_id
                == ns.key(KeyDomain::AssessmentRevision, &[&value.session, &value.job])?
            && revision.selected_on_submission == (value.status == Status::Completed)
            && (!revision.selected_on_submission || revision.supersedes == value.expected_revision)
            && (if revision.selected_on_submission {
                revision.selection_reason == "selected"
            } else {
                revision.supersedes.is_none()
                    && matches!(
                        revision.selection_reason.as_str(),
                        "authority_changed"
                            | "head_changed"
                            | "human_revision_preserved"
                            | "stopped_owner_recovery"
                    )
            }),
        "native paid revision selection changed"
    );
    budget.charge(0, io::json_len(completion.packet(), 8 * 1024 * 1024)?)?;
    source_identity(&value, completion)?;
    ensure!(
        value.job
            == ns.key(
                KeyDomain::LearningJob,
                &[
                    value.configuration.as_str(),
                    &value.session,
                    &value.checkpoint,
                    &io::json(&value.route, 4096)?,
                    &value.input_digest,
                    judge::NATIVE_JUDGE_VERSION,
                ]
            )?,
        "native paid logical job key changed"
    );
    let input = revision_input(&value, completion.packet())?;
    ensure!(io::json(&revision.input, RECORD_BYTES)? == io::json(&input, RECORD_BYTES)?
        && value.history_reservation.as_ref().is_some_and(|r| matches!(r.state,
            ReservationState::Consumed { actual_bytes } if io::json_len(revision, RECORD_BYTES).is_ok_and(|n| n == actual_bytes))),
        "native paid revision output changed");
    let recovery = value
        .recovery
        .as_ref()
        .context("native terminal provenance absent")?;
    verify_receipt(
        tx,
        &recovery.seed,
        &recovery
            .observed
            .as_ref()
            .context("native terminal observation absent")?
            .receipt_digest,
        budget,
    )
    .await?;
    Ok(())
}
