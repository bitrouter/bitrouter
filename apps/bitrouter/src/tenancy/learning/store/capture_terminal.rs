//! Read-only paid ACP source closure inside the original caller's H snapshot.
use super::*;
use crate::{
    core::evolution::{evidence::EvidencePacket, scoring},
    storage::evolution::digest,
};
use bitrouter_ai::tenancy::execution::{Outcome, ReceiptState};

/// Evidence only: neither this projection nor its stored seed grants authority.
pub(in crate::tenancy) struct Closure {
    pub digest: String,
    pub complete: bool,
}

pub(in crate::tenancy) async fn validate_revision(
    tx: &DatabaseTransaction,
    a: &Association,
    evaluator: &ResourceRef,
    session: &str,
    packet: &EvidencePacket,
    revision: &AssessmentRevision,
    budget: &OperationBudget<'_>,
) -> Result<Closure> {
    let row = record(tx, a, JOB, &revision.input.submission_id, budget)
        .await?
        .context("Block paid terminal job absent")?;
    a.validate(
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
    )?;
    budget.charge(
        0,
        row.body
            .len()
            .checked_mul(2)
            .context("paid terminal decode bound")?,
    )?;
    let value: Record = serde_json::from_str(&row.body)?;
    ensure!(
        valid_record(&value)
            && value.version == 3
            && row.record_id == value.job
            && row.kind == JOB
            && row.revision == record_revision(&value)
            && row.session_key.as_deref() == Some(session)
            && value.session == session
            && value.configuration.as_str() == a.descriptor().revision
            && value.route == *evaluator
            && value.status == Status::Completed
            && value.assessment.as_deref() == Some(revision.revision_id.as_str())
            && value.job == revision.input.submission_id
            && revision.input.source == AssessmentSource::Agentic
            && revision.selected_on_submission
            && revision.selection_reason == "selected"
            && revision.supersedes == value.expected_revision
            && value.checkpoint == packet.checkpoint_id
            && value.prefix_digest == packet.prefix_digest
            && packet.projection_version == crate::core::evolution::evidence::EVIDENCE_VERSION
            && io::json(&value, RECORD_BYTES)? == row.body,
        "Block paid terminal identity changed"
    );
    let recovery = value
        .recovery
        .as_ref()
        .context("Block paid provenance absent")?;
    ensure!(
        recovery.seed.descriptor.as_ref() == Some(a.descriptor())
            && recovery.seed.connection.as_deref() == Some(a.connection())
            && recovery.seed.native.is_none(),
        "Block paid capture source changed"
    );
    ensure!(
        value.job
            == a.key(
                KeyDomain::LearningJob,
                &[
                    value.configuration.as_str(),
                    session,
                    &value.checkpoint,
                    &io::json(evaluator, 4096)?,
                    &value.input_digest,
                    judge::JUDGE_VERSION,
                ]
            )?
            && revision.revision_id
                == a.key(KeyDomain::AssessmentRevision, &[session, &value.job])?,
        "Block paid source key changed"
    );
    // Charge the retained projection before copying it into the shared judge
    // digest algorithm. This shares the caller's 4096-row/32MiB/5s budget.
    budget.charge(0, io::json_len(packet, 12 * 1024 * 1024)?)?;
    let input = ScoringInput {
        rubric_version: rubric::RUBRIC_VERSION,
        library: rubric::library(),
        expected_revision: value.expected_revision.clone(),
        evidence: packet.clone(),
    };
    ensure!(
        judge::input_digest(&input, evaluator.resource_id.as_str())? == value.input_digest,
        "Block paid accepted input changed"
    );
    let output = value.output.as_ref().context("Block paid output absent")?;
    ensure!(
        output.request_id == value.execution.as_str()
            && output.model == evaluator.resource_id.as_str()
            && output.judge_version == judge::JUDGE_VERSION
            && output.input_digest == value.input_digest,
        "Block paid output identity changed"
    );
    let identity = io::json(evaluator, 4096)?;
    let contract =
        scoring::measurement_contract(AssessmentSource::Agentic, &identity, judge::JUDGE_VERSION)?;
    let expected = RevisionInput {
        submission_id: value.job.clone(),
        checkpoint_id: value.checkpoint.clone(),
        expected_revision: value.expected_revision.clone(),
        source: AssessmentSource::Agentic,
        evaluator_id: identity,
        evaluator_version: judge::JUDGE_VERSION.into(),
        assessment: Some(output.evaluation.assessment(packet, &contract)?),
        reason: output.evaluation.summary.clone(),
    };
    ensure!(io::json(&revision.input, RECORD_BYTES)? == io::json(&expected, RECORD_BYTES)?
        && revision.input_digest == digest(&expected)?
        && value.history_reservation.as_ref().is_some_and(|r| matches!(r.state,
            ReservationState::Consumed { actual_bytes } if io::json_len(revision, RECORD_BYTES).is_ok_and(|n| n == actual_bytes))),
        "Block paid output/history projection changed");
    ensure!(
        record(
            tx,
            a,
            PENDING,
            &a.key(KeyDomain::LearningPending, &[&value.job])?,
            budget
        )
        .await?
        .is_none(),
        "Block paid terminal retained a pending obligation"
    );
    let observed = recovery
        .observed
        .as_ref()
        .context("Block paid observation absent")?;
    // Same bounded035 decoder and allowance used by paid observation/finish.
    // Incomplete usage remains retained, not invented as zero or corruption.
    budget.charge(
        4,
        1024 * 1024 + crate::tenancy::receipts::auxiliary::MAX_BYTES + 8192,
    )?;
    let receipt = crate::tenancy::receipts::store::lookup_candidate(
        tx,
        &recovery.seed.installation,
        &recovery.seed.tenant,
        &value.execution,
    )
    .await?
    .context("Block paid judge receipt absent")?;
    budget.check()?;
    recovery.seed.receipt(&receipt)?;
    ensure!(
        receipt.learning_digest() == observed.receipt_digest
            && observed.output_digest == digest(output)?,
        "Block paid observed result changed"
    );
    Ok(Closure {
        digest: digest(&(
            &row.record_id,
            row.revision,
            &row.body,
            receipt.learning_digest(),
        ))?,
        complete: receipt.receipt.state == ReceiptState::Committed
            && receipt.receipt.outcome == Outcome::Completed
            && receipt.receipt.usage.complete
            && !receipt.auxiliary_unknown()
            && !receipt.auxiliary_unclean(),
    })
}
