use super::*;
use crate::{
    harness::trajectory::owned::{entities::checkpoints, prefix::Prefix},
    storage::scope::{Provenance, Scope},
    tenancy::{
        configuration_store, learning::recovery::Seed,
        resources::execution::RetainedExecutionRevision,
    },
};
use bitrouter_ai::tenancy::{configuration::LearningMode, identity::InstallationId};
use sea_orm::{DatabaseBackend, Statement, sea_query::Query};

pub(super) struct Configuration {
    pub execution: RetainedExecutionRevision,
    pub maximum: u32,
    pub digest: String,
}
pub(super) async fn configuration(
    tx: &DatabaseTransaction,
    installation: &InstallationId,
    tenant: &TenantId,
    revision: &RevisionId,
    resources: &Resources,
    budget: &OperationBudget<'_>,
    prior: Option<&Arc<Configuration>>,
) -> Result<Arc<Configuration>> {
    budget.charge(1, 256)?;
    let row = tx.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT CAST(length(CAST(document_json AS BLOB))+length(CAST(compiled_json AS BLOB))+length(CAST(dependencies_json AS BLOB)) AS BIGINT) AS payload_bytes FROM tenancy_configurations WHERE tenant_id=? AND revision_id=?",
        [tenant.as_str().into(), revision.as_str().into()])).await?.context("paid retained configuration absent")?;
    budget.check()?;
    let bytes = usize::try_from(row.try_get::<i64>("", "payload_bytes")?)?;
    // Bound transferred/parsed JSON and fingerprint work before decoding. The
    // first pass charges actual target/native expansions inside their original
    // per-revision 8 MiB quotas, against this same cumulative operation. Exact
    // recheck reuses the decode ONLY after all persisted bytes match.
    // This is projected-work accounting, not an allocator/RSS claim.
    budget.charge(
        1,
        bytes
            .checked_mul(3)
            .and_then(|v| v.checked_add(4096))
            .context("paid retained configuration bound")?,
    )?;
    let saved = configuration_store::configuration(tx, tenant, revision)
        .await?
        .context("paid retained configuration disappeared")?;
    budget.check()?;
    let digest = crate::storage::evolution::digest(&(
        &saved.revision,
        &saved.compiled,
        &saved.dependencies,
        &saved.creator,
        saved.authority_revision,
        &saved.created_at,
    ))?;
    if let Some(prior) = prior {
        ensure!(
            prior.digest == digest,
            "paid retained configuration changed"
        );
        return Ok(prior.clone());
    }
    let execution = resources.open_paid_recovery(&saved, budget)?;
    ensure!(
        execution.installation_id() == installation
            && execution.tenant_id() == tenant
            && execution.revision_id() == revision
            && execution.document().learning.mode != LearningMode::Off,
        "paid retained configuration binding changed"
    );
    let maximum = execution.document().learning.max_evaluations;
    ensure!(maximum > 0, "paid retained budget is zero");
    Ok(Arc::new(Configuration {
        execution,
        maximum,
        digest,
    }))
}

pub(super) struct Closure {
    pub association: namespace::Stored,
    pub subject: String,
    pub input: ScoringInput,
    pub digest: String,
}

pub(super) async fn job(
    tx: &DatabaseTransaction,
    row: &records::Model,
    value: &Record,
    namespace: (Scope, Provenance),
    profile: &Configuration,
    identity: &crate::tenancy::learning::recovery::RecoveryIdentity,
    budget: &OperationBudget<'_>,
) -> Result<Closure> {
    let (scope, provenance) = namespace;
    if value.version == 4 {
        return super::native::job(tx, value, scope, provenance, profile, identity, budget).await;
    }
    let checkpoint = io::one(
        tx,
        checkpoints::Entity::find_by_id(&value.checkpoint)
            .filter(scope.predicate(
                (Alias::new("acp_checkpoints"), Alias::new("installation_id")),
                (Alias::new("acp_checkpoints"), Alias::new("tenant_id")),
            ))
            .filter(checkpoints::Column::CreatorPrincipalId.eq(row.creator_principal_id.clone())),
        256 * 1024 + 4096,
        budget,
    )
    .await?
    .context("paid checkpoint unavailable")?;
    let prefix: Prefix = assessment::recovery_prefix(&checkpoint.manifest_json)?;
    let d = &prefix.descriptor;
    let mut association = Association::new(
        scope,
        provenance,
        d.controller.clone(),
        d.source.clone(),
        d.revision.clone(),
        d.model.clone(),
        d.mode,
    )?;
    if let Some(link) = &d.block_link {
        association = association.with_block_link(link.clone())?;
    }
    ensure!(
        association.descriptor() == d
            && association.connection() == prefix.connection
            && prefix.session_key == value.session
            && d.revision == value.configuration.as_str()
            && d.mode == profile.execution.document().learning.mode,
        "paid checkpoint namespace differs from accepted record"
    );
    if value.status == Status::Accepted {
        // Check the entire durable correlation chain, including any post-prefix
        // disconnect, before using its separately acknowledged local cleanup.
        // This returns data only; it does not recreate a live controller.
        association
            .stopped_prefix_without_tools_in(tx, &prefix.native, budget)
            .await?;
    }
    let (session, summary, _, evidence) = assessment::review_in(
        tx,
        &association,
        &prefix.native,
        Some(&value.checkpoint),
        budget,
    )
    .await?;
    ensure!(
        session == value.session && summary.prefix_digest == value.prefix_digest,
        "paid historical evidence changed"
    );
    let input = ScoringInput {
        rubric_version: rubric::RUBRIC_VERSION,
        library: rubric::library(),
        expected_revision: value.expected_revision.clone(),
        evidence,
    };
    ensure!(
        judge::input_digest(&input, value.route.resource_id.as_str())? == value.input_digest,
        "paid input/rubric contract changed"
    );
    let evaluator = io::json(&value.route, 4096)?;
    ensure!(
        value.job
            == association.key(
                KeyDomain::LearningJob,
                &[
                    value.configuration.as_str(),
                    &session,
                    &value.checkpoint,
                    &evaluator,
                    &value.input_digest,
                    judge::JUDGE_VERSION
                ]
            )?,
        "paid logical job key changed"
    );
    chrono::DateTime::parse_from_rfc3339(&value.accepted_at)?;
    if let Some(output) = &value.output {
        ensure!(
            output.request_id == value.execution.as_str()
                && output.model == value.route.resource_id.as_str()
                && output.judge_version == judge::JUDGE_VERSION
                && output.input_digest == value.input_digest,
            "paid retained answer identity changed"
        );
        output.evaluation.aggregate(&input.evidence)?;
    }
    // Current history is read only to verify existing obligations and exact
    // replay. Recovery never refreshes expected_revision or selects a head.
    let history = history::load(tx, &association, &session, &value.checkpoint, budget).await?;
    let owed = reserved_in(tx, &association, &session, &value.checkpoint, budget).await?;
    history::admit(
        &summary,
        &input,
        &history,
        owed,
        history::Allowance::default(),
    )?;
    let current = head(tx, &association, &session, budget).await?;
    let current_revision = if let Some(id) = current.as_ref().and_then(|v| v.revision_id.as_ref()) {
        Some(
            history::revision(tx, &association, &session, id, budget)
                .await?
                .context("paid current assessment head absent")?,
        )
    } else {
        None
    };
    let stamp = (&summary, &input, &history.values, &current_revision);
    let bytes = io::json_len(&stamp, 16 * 1024 * 1024)?;
    budget.charge(0, bytes)?;
    let digest = crate::storage::evolution::digest(&stamp)?;
    Ok(Closure {
        subject: association.descriptor().model.clone(),
        association: namespace::Stored::Capture(Box::new(association)),
        input,
        digest,
    })
}

pub(super) async fn receipt(
    tx: &DatabaseTransaction,
    seed: &Seed,
    budget: &OperationBudget<'_>,
) -> Result<Option<Candidate>> {
    // Reject auxiliary evidence by existence only before its potentially large
    // decoder runs. This profile cannot prove OAuth/Count or tool completion.
    budget.charge(1, 256)?;
    let query = Query::select()
        .expr(Expr::val(1))
        .from(Alias::new("tenancy_receipt_auxiliary"))
        .and_where(Expr::col(Alias::new("installation_id")).eq(seed.installation.as_str()))
        .and_where(Expr::col(Alias::new("tenant_id")).eq(seed.tenant.as_str()))
        .and_where(Expr::col(Alias::new("execution_id")).eq(seed.execution.as_str()))
        .limit(1)
        .to_owned();
    ensure!(
        tx.query_one(tx.get_database_backend().build(&query))
            .await?
            .is_none(),
        "paid auxiliary effect is uncovered"
    );
    budget.charge(2, 1024 * 1024 + 8192)?;
    let candidate = crate::tenancy::receipts::store::lookup_candidate(
        tx,
        &seed.installation,
        &seed.tenant,
        &seed.execution,
    )
    .await?;
    budget.check()?;
    if let Some(candidate) = &candidate {
        seed.receipt(candidate)?;
        ensure!(!candidate.has_auxiliary(), "paid auxiliary changed");
    }
    Ok(candidate)
}

pub(super) async fn terminal(
    tx: &DatabaseTransaction,
    namespace: &namespace::Stored,
    value: &Record,
    input: &ScoringInput,
    budget: &OperationBudget<'_>,
) -> Result<String> {
    let a = match namespace {
        namespace::Stored::Native(a) => {
            return super::native::terminal(tx, a, value, input, budget).await;
        }
        namespace::Stored::Capture(a) => a,
    };
    let mut terminal_receipt = None;
    if let Some(recovery) = &value.recovery {
        let saved = receipt(tx, &recovery.seed, budget)
            .await?
            .context("terminal paid receipt missing")?;
        if let Some(observed) = &recovery.observed {
            ensure!(
                saved.digest == observed.receipt_digest,
                "terminal paid receipt changed"
            );
        }
        terminal_receipt = Some(saved.digest);
    }
    match (&value.assessment, &value.output) {
        (Some(id), Some(output)) => {
            let revision = history::revision(tx, a, &value.session, id, budget)
                .await?
                .context("terminal paid assessment absent")?;
            let evaluator = io::json(&value.route, 4096)?;
            let pipeline = crate::core::evolution::scoring::measurement_contract(
                AssessmentSource::Agentic,
                &evaluator,
                judge::JUDGE_VERSION,
            )?;
            let expected = RevisionInput {
                submission_id: value.job.clone(),
                checkpoint_id: value.checkpoint.clone(),
                expected_revision: value.expected_revision.clone(),
                source: AssessmentSource::Agentic,
                evaluator_id: evaluator,
                evaluator_version: judge::JUDGE_VERSION.into(),
                assessment: Some(output.evaluation.assessment(&input.evidence, &pipeline)?),
                reason: output.evaluation.summary.clone(),
            };
            ensure!(
                io::json(&revision.input, RECORD_BYTES)? == io::json(&expected, RECORD_BYTES)?
                    && revision.revision_id == a.key(KeyDomain::AssessmentRevision, &[&value.session, &value.job])?
                    && value.history_reservation.as_ref().is_none_or(|reservation| matches!(
                        reservation.state, ReservationState::Consumed { actual_bytes }
                            if io::json_len(&revision, RECORD_BYTES).is_ok_and(|bytes| actual_bytes == bytes)
                    ))
                    && revision.input.submission_id == value.job
                    && revision.input.checkpoint_id == value.checkpoint
                    && revision.input.source == AssessmentSource::Agentic
                    && revision.input.evaluator_id == io::json(&value.route, 4096)?
                    && revision.input.evaluator_version == output.judge_version
                    && revision.selected_on_submission == (value.status == Status::Completed),
                "terminal paid assessment link changed"
            );
        }
        (None, None) => ensure!(
            matches!(value.status, Status::Failed | Status::Interrupted),
            "terminal paid result missing"
        ),
        _ => anyhow::bail!("terminal paid answer link mismatch"),
    }
    crate::storage::evolution::digest(&terminal_receipt)
}
