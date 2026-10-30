//! Bootstrap audit of native paid provenance, never reconstruction of a live owner.
use super::*;
use crate::{
    storage::scope::{Provenance, Scope},
    tenancy::{
        experiment::store::completed,
        learning::recovery::{RecoveryIdentity, Retained},
        session::assessment::store as assessments,
    },
};

pub(super) async fn job(
    tx: &DatabaseTransaction,
    value: &Record,
    scope: Scope,
    provenance: Provenance,
    profile: &closure::Configuration,
    identity: &RecoveryIdentity,
    budget: &OperationBudget<'_>,
) -> Result<closure::Closure> {
    let retained = value
        .recovery
        .as_ref()
        .context("native paid provenance absent")?;
    let native = retained
        .seed
        .native
        .as_ref()
        .context("native paid source absent")?;
    let packet = value
        .native_evidence
        .as_ref()
        .context("native paid projection absent")?;
    budget.charge(
        0,
        io::json_len(packet, RECORD_BYTES)?
            .checked_mul(3)
            .context("native audit projection bound")?,
    )?;
    let completion = completed::audit_paid(tx, identity, native, packet.clone(), budget).await?;
    completed::check_retained(&profile.execution, completion.profile())?;
    super::super::native::terminal::source_identity(value, &completion)?;
    let subject = completion.profile().route.resource_id.as_str().to_owned();
    let ns = namespace::Native::new(scope.clone(), provenance);
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
        "native paid historical logical key changed"
    );
    let input = ScoringInput {
        rubric_version: rubric::RUBRIC_VERSION,
        library: rubric::library(),
        expected_revision: value.expected_revision.clone(),
        evidence: packet.clone(),
    };
    if let Some(output) = &value.output {
        ensure!(
            output.request_id == value.execution.as_str()
                && output.model == value.route.resource_id.as_str()
                && output.judge_version == judge::NATIVE_JUDGE_VERSION
                && output.input_digest == value.input_digest,
            "native retained paid answer changed"
        );
        output.evaluation.aggregate(packet)?;
    }
    let view = assessments::load(tx, &scope, completion, budget).await?;
    ensure!(
        view.has_source(),
        "native paid source was not retained at acceptance"
    );
    let owed =
        super::super::native::reserved(tx, &ns, &value.session, &value.checkpoint, budget).await?;
    assessments::reserve_paid(&view, owed, false, budget)?;
    let stamp = (
        view.completion.edge(),
        &input,
        &view.history,
        &view.selected,
    );
    budget.charge(0, io::json_len(&stamp, 16 * 1024 * 1024)?)?;
    let digest = crate::storage::evolution::digest(&stamp)?;
    Ok(closure::Closure {
        association: namespace::Stored::Native(ns),
        input,
        digest,
        subject,
    })
}

pub(super) async fn cleanup(
    tx: &DatabaseTransaction,
    identity: &RecoveryIdentity,
    retained: &Retained,
    budget: &OperationBudget<'_>,
) -> Result<String> {
    let native = retained
        .seed
        .native
        .as_ref()
        .context("native paid source absent")?;
    let acknowledged = retained
        .native_cleanup
        .as_ref()
        .context("native local close ACK absent")?;
    let (completion, journal_digest) =
        completed::stopped_paid(tx, identity, native, budget).await?;
    ensure!(
        acknowledged.origin == retained.seed.origin
            && acknowledged.descriptor == native.descriptor
            && acknowledged.grant == native.grant
            && acknowledged.head == completion.edge().head
            && acknowledged.journal_digest == journal_digest,
        "native actual local cleanup ACK changed"
    );
    crate::storage::evolution::digest(acknowledged)
}

pub(super) async fn terminal(
    tx: &DatabaseTransaction,
    ns: &namespace::Native,
    value: &Record,
    input: &ScoringInput,
    budget: &OperationBudget<'_>,
) -> Result<String> {
    let retained = value
        .recovery
        .as_ref()
        .context("native paid provenance absent")?;
    let saved = closure::receipt(tx, &retained.seed, budget)
        .await?
        .context("native terminal receipt absent")?;
    if let Some(observed) = &retained.observed {
        ensure!(
            observed.receipt_digest == saved.digest,
            "native terminal receipt changed"
        );
    }
    match (&value.assessment, &value.output) {
        (Some(id), Some(_)) => {
            let row = io::one(
                tx,
                revisions::Entity::find_by_id(id).filter(ns.scope().predicate(
                    revisions::Column::InstallationId,
                    revisions::Column::TenantId,
                )),
                RECORD_BYTES + 4096,
                budget,
            )
            .await?
            .context("native terminal revision absent")?;
            ns.validate(
                row.installation_id.as_deref(),
                row.tenant_id.as_deref(),
                row.creator_principal_id.as_deref(),
            )?;
            let revision: AssessmentRevision = serde_json::from_str(&row.revision_json)?;
            let expected = super::super::native::terminal::revision_input(value, &input.evidence)?;
            ensure!(row.session_key == value.session && row.checkpoint_id == value.checkpoint
                && row.created_at == revision.created_at && row.revision_id == revision.revision_id
                && revision.revision_id == ns.key(KeyDomain::AssessmentRevision, &[&value.session, &value.job])?
                && io::json(&revision, RECORD_BYTES)? == row.revision_json
                && io::json(&revision.input, RECORD_BYTES)? == io::json(&expected, RECORD_BYTES)?
                && revision.selected_on_submission == (value.status == Status::Completed)
                && value.history_reservation.as_ref().is_some_and(|r| matches!(r.state,
                    ReservationState::Consumed { actual_bytes } if actual_bytes == row.revision_json.len())),
                "native terminal revision identity changed");
        }
        (None, None) => ensure!(
            matches!(value.status, Status::Failed | Status::Interrupted),
            "native terminal state changed"
        ),
        _ => anyhow::bail!("native terminal answer link mismatch"),
    }
    crate::storage::evolution::digest(&saved.digest)
}
