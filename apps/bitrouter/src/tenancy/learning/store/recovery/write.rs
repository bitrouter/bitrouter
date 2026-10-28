use super::*;

pub(super) fn assessment(
    a: &namespace::Stored,
    value: &Record,
    input: &ScoringInput,
    at: &Timestamp,
    budget: &OperationBudget<'_>,
) -> Result<Option<AssessmentRevision>> {
    let Some(output) = &value.output else {
        return Ok(None);
    };
    ensure!(
        value
            .recovery
            .as_ref()
            .is_some_and(|r| r.observed.is_some()),
        "paid output lacks observed ACK"
    );
    let evaluator = io::json(&value.route, 4096)?;
    let version = if value.version == 4 {
        judge::NATIVE_JUDGE_VERSION
    } else {
        judge::JUDGE_VERSION
    };
    let pipeline = crate::core::evolution::scoring::measurement_contract_for(
        &input.evidence.projection_version,
        AssessmentSource::Agentic,
        &evaluator,
        version,
    )?;
    let input = RevisionInput {
        submission_id: value.job.clone(),
        checkpoint_id: value.checkpoint.clone(),
        expected_revision: value.expected_revision.clone(),
        source: AssessmentSource::Agentic,
        evaluator_id: evaluator,
        evaluator_version: version.into(),
        assessment: Some(output.evaluation.assessment(&input.evidence, &pipeline)?),
        reason: output.evaluation.summary.clone(),
    };
    let revision = AssessmentRevision {
        revision_id: a.key(KeyDomain::AssessmentRevision, &[&value.session, &value.job])?,
        input_digest: crate::storage::evolution::digest(&input)?,
        input,
        supersedes: None,
        selected_on_submission: false,
        selection_reason: "stopped_owner_recovery".into(),
        created_at: at.as_str().to_owned(),
    };
    budget.charge(0, io::json_len(&revision, RECORD_BYTES)?)?;
    Ok(Some(revision))
}

pub(super) async fn apply(
    tx: &DatabaseTransaction,
    plan: &Plan,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    for write in &plan.writes {
        let a = &write.association;
        if write.missing_receipt {
            budget.charge(
                1,
                usize::try_from(write.receipt.bytes())?
                    .checked_mul(2)
                    .context("paid terminal size")?,
            )?;
            ensure!(
                crate::tenancy::receipts::store::insert(tx, &write.receipt).await?,
                "paid missing receipt appeared during repair"
            );
        }
        if let Some(revision) = &write.assessment {
            let body = io::json(revision, RECORD_BYTES)?;
            budget.charge(1, body.len() + 4096)?;
            let meta = a.metadata()?;
            ensure!(
                revisions::Entity::insert(revisions::ActiveModel {
                    revision_id: Set(revision.revision_id.clone()),
                    session_key: Set(write.terminal.session.clone()),
                    checkpoint_id: Set(write.terminal.checkpoint.clone()),
                    created_at: Set(revision.created_at.clone()),
                    revision_json: Set(body),
                    installation_id: Set(meta.installation),
                    tenant_id: Set(meta.tenant),
                    creator_principal_id: Set(meta.creator),
                })
                .exec_without_returning(tx)
                .await?
                    == 1,
                "paid recovery assessment conflict"
            );
        }
        save_as(tx, a, &write.row, JOB, &write.terminal, budget).await?;
        budget.charge(1, 4096)?;
        ensure!(
            records::Entity::delete_many()
                .filter(scope(a))
                .filter(records::Column::RecordId.eq(&write.pending.record_id))
                .filter(records::Column::Kind.eq(PENDING))
                .filter(records::Column::Revision.eq(write.pending.revision))
                .filter(records::Column::Body.eq(&write.pending.body))
                .exec(tx)
                .await?
                .rows_affected
                == 1,
            "paid recovery pending CAS failed"
        );
        budget.check()?;
    }
    Ok(())
}
