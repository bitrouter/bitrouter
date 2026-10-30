//! Native rows borrow H; no ACP session or checkpoint is manufactured.
use super::{Input, Reply};
use crate::{
    acp_trajectory::checkpoint::types::{AssessmentRevision, AssessmentSource, RevisionInput},
    core::evolution::{
        rubric,
        scoring::{self, ScoringInput, ScoringReceipt},
    },
    harness::{
        resources::managed::session::assessment::NativeRunReview,
        trajectory::owned::{entities::records, io},
    },
    storage::{
        operation_budget::{Error, OperationBudget},
        scope::{KeyDomain, MetadataRef, Provenance, Scope},
    },
    tenancy::{
        execution::session::Binding,
        experiment::store::completed::{self, Completion, Edge, Reader},
        learning::{
            entities::{heads, revisions},
            history,
        },
    },
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::operation::ErrorCode;
use sea_orm::{
    ColumnTrait, DatabaseTransaction, EntityTrait, QueryFilter, QueryOrder, QuerySelect, Set,
    sea_query::Expr,
};

const SOURCE_KIND: &str = "native_assessment_source_v1";
const SOURCE_BYTES: usize = 64 * 1024;
const HISTORY_ROWS: usize = 128;
const HISTORY_BYTES: usize = 8 * 1024 * 1024;
const REVIEW_BYTES: usize = 16 * 1024 * 1024;

pub(in crate::tenancy) struct View {
    pub completion: Completion,
    pub selected: Option<AssessmentRevision>,
    pub history: Vec<AssessmentRevision>,
    head: Option<heads::Model>,
    source: Option<records::Model>,
    bytes: usize,
}

impl View {
    pub(in crate::tenancy) fn has_source(&self) -> bool {
        self.source.is_some()
    }
}

fn source_key(scope: &Scope, unit: &str) -> Result<String> {
    scope.physical_key(
        KeyDomain::AssessmentRevision,
        "",
        &[unit, "native-source-v1"],
    )
}
fn revision_key(scope: &Scope, unit: &str, submission: &str) -> Result<String> {
    scope.physical_key(
        KeyDomain::AssessmentRevision,
        "",
        &[unit, "native-human-v1", submission],
    )
}
fn metadata(
    scope: &Scope,
    installation: Option<&str>,
    tenant: Option<&str>,
    creator: Option<&str>,
    expected: &str,
) -> Result<()> {
    scope.validate_metadata(MetadataRef {
        installation,
        tenant,
        creator,
    })?;
    ensure!(
        creator == Some(expected),
        "native assessment stable principal changed"
    );
    Ok(())
}
fn validate_revision(
    scope: &Scope,
    completion: &Completion,
    row: &revisions::Model,
) -> Result<AssessmentRevision> {
    metadata(
        scope,
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
        completion.principal(),
    )?;
    let edge = completion.edge();
    ensure!(
        row.session_key == edge.unit
            && row.checkpoint_id == edge.checkpoint
            && row.revision_json.len() <= history::REVISION_BYTES,
        "native assessment parent changed"
    );
    let revision: AssessmentRevision = serde_json::from_str(&row.revision_json)?;
    if revision.input.source == AssessmentSource::Agentic {
        ensure!(
            row.revision_id == revision.revision_id
                && row.created_at == revision.created_at
                && revision.input.checkpoint_id == edge.checkpoint
                && revision.input_digest == crate::storage::evolution::digest(&revision.input)?
                && io::json(&revision, history::REVISION_BYTES)? == row.revision_json,
            "native paid assessment row changed"
        );
        chrono::DateTime::parse_from_rfc3339(&revision.created_at)?;
        return Ok(revision);
    }
    ensure!(
        revision.input.source == AssessmentSource::Human
            && revision.input.evaluator_id == format!("owned-human/{}", completion.principal())
            && revision.input.evaluator_version == "owned-manual-rubric-v1"
            && revision.selection_reason == "selected"
            && revision.revision_id
                == revision_key(scope, &edge.unit, &revision.input.submission_id)?
            && row.revision_id == revision.revision_id
            && row.created_at == revision.created_at
            && revision.input.checkpoint_id == edge.checkpoint
            && revision.selected_on_submission
            && revision.supersedes == revision.input.expected_revision
            && revision.input_digest == crate::storage::evolution::digest(&revision.input)?
            && io::json(&revision, history::REVISION_BYTES)? == row.revision_json,
        "native assessment revision identity changed"
    );
    chrono::DateTime::parse_from_rfc3339(&revision.created_at)?;
    if let Some(content) = &revision.input.assessment {
        let evaluation: rubric::RubricEvaluation = serde_json::from_str(&content.explanation)?;
        let pipeline = scoring::measurement_contract_for(
            &completion.packet().projection_version,
            revision.input.source,
            &revision.input.evaluator_id,
            &revision.input.evaluator_version,
        )?;
        let expected = evaluation.assessment(completion.packet(), &pipeline)?;
        ensure!(
            io::json(&expected, history::REVISION_BYTES)?
                == io::json(content, history::REVISION_BYTES)?,
            "native assessment rubric differs from its complete source"
        );
    }
    Ok(revision)
}

async fn source(
    tx: &DatabaseTransaction,
    scope: &Scope,
    unit: &str,
    budget: &OperationBudget<'_>,
) -> Result<Option<records::Model>> {
    // Exact ID first; a differently typed occupant is corruption, not absence.
    let row = io::one(
        tx,
        records::Entity::find_by_id(source_key(scope, unit)?)
            .filter(scope.predicate(records::Column::InstallationId, records::Column::TenantId))
            .lock_exclusive(),
        SOURCE_BYTES + 4096,
        budget,
    )
    .await?;
    if let Some(row) = &row {
        ensure!(
            row.kind == SOURCE_KIND && row.scope_id == unit && row.revision == 0,
            "native assessment source type changed"
        );
    }
    Ok(row)
}

async fn view(
    tx: &DatabaseTransaction,
    scope: &Scope,
    completion: Completion,
    source: Option<records::Model>,
    budget: &OperationBudget<'_>,
) -> Result<View> {
    let edge = completion.edge();
    if let Some(row) = &source {
        metadata(
            scope,
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
            completion.principal(),
        )?;
        let retained: Edge = serde_json::from_str(&row.body)?;
        ensure!(
            row.session_key.as_deref() == Some(edge.session.as_str())
                && completion.matches_source(&retained)
                && io::json(&retained, SOURCE_BYTES)? == row.body,
            "native assessment immutable source changed"
        );
    }
    let head = io::one(
        tx,
        heads::Entity::find_by_id(&edge.unit)
            .filter(scope.predicate(heads::Column::InstallationId, heads::Column::TenantId))
            .lock_exclusive(),
        4096,
        budget,
    )
    .await?;
    let rows = io::rows(
        tx,
        revisions::Entity::find()
            .filter(scope.predicate(
                revisions::Column::InstallationId,
                revisions::Column::TenantId,
            ))
            .filter(revisions::Column::SessionKey.eq(&edge.unit))
            .order_by_asc(revisions::Column::CreatedAt)
            .order_by_asc(revisions::Column::RevisionId)
            .lock_exclusive(),
        HISTORY_ROWS,
        HISTORY_BYTES + HISTORY_ROWS * 4096,
        budget,
    )
    .await?;
    let mut bytes = 0usize;
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        budget.check()?;
        bytes = bytes
            .checked_add(row.revision_json.len())
            .ok_or(Error::Capacity)?;
        ensure!(bytes <= HISTORY_BYTES, Error::Capacity);
        let value = validate_revision(scope, &completion, &row)?;
        if value.input.source == AssessmentSource::Agentic {
            crate::tenancy::learning::store::native::terminal::validate_revision(
                tx,
                scope,
                &completion,
                &value,
                budget,
            )
            .await?;
        }
        values.push(value);
    }
    let selected = if let Some(head) = &head {
        metadata(
            scope,
            head.installation_id.as_deref(),
            head.tenant_id.as_deref(),
            head.creator_principal_id.as_deref(),
            completion.principal(),
        )?;
        let id = head
            .revision_id
            .as_deref()
            .context("native assessment head is null")?;
        ensure!(source.is_some(), "native assessment head has no source");
        Some(
            values
                .iter()
                .find(|value| value.revision_id == id)
                .context("native assessment head revision missing")?
                .clone(),
        )
    } else {
        ensure!(
            values.iter().all(|value| !value.selected_on_submission)
                && (values.is_empty() || source.is_some()),
            "native selected assessment rows lack effective head"
        );
        None
    };
    // Every supersedes edge stays in this exact run's immutable history.
    for value in &values {
        if let Some(previous) = &value.supersedes {
            ensure!(
                previous != &value.revision_id
                    && values.iter().any(|entry| &entry.revision_id == previous && entry.selected_on_submission),
                "native assessment history parent missing"
            );
        }
    }
    let mut visited = std::collections::BTreeSet::new();
    let mut next = selected.as_ref();
    while let Some(value) = next {
        ensure!(
            visited.insert(value.revision_id.as_str()),
            "native assessment history cycle"
        );
        next = value
            .supersedes
            .as_ref()
            .map(|id| {
                values
                    .iter()
                    .find(|entry| &entry.revision_id == id)
                    .context("native assessment predecessor missing")
            })
            .transpose()?;
    }
    ensure!(
        visited.len()
            == values
                .iter()
                .filter(|value| value.selected_on_submission)
                .count(),
        "native assessment history is not the effective CAS chain"
    );
    Ok(View {
        completion,
        selected,
        history: values,
        head,
        source,
        bytes,
    })
}

pub(in crate::tenancy) async fn load(
    tx: &DatabaseTransaction,
    scope: &Scope,
    completion: Completion,
    budget: &OperationBudget<'_>,
) -> Result<View> {
    let row = source(tx, scope, &completion.edge().unit, budget).await?;
    view(tx, scope, completion, row, budget).await
}

pub(super) async fn selected(
    tx: &DatabaseTransaction,
    scope: &Scope,
    unit: &str,
    reader: Reader<'_>,
    budget: &OperationBudget<'_>,
) -> Result<Option<View>> {
    let Some(row) = source(tx, scope, unit, budget).await? else {
        let head = io::one(
            tx,
            heads::Entity::find_by_id(unit)
                .filter(scope.predicate(heads::Column::InstallationId, heads::Column::TenantId)),
            4096,
            budget,
        )
        .await?;
        ensure!(head.is_none(), "native assessment head has no source");
        let orphan = io::rows(
            tx,
            revisions::Entity::find()
                .filter(scope.predicate(
                    revisions::Column::InstallationId,
                    revisions::Column::TenantId,
                ))
                .filter(revisions::Column::SessionKey.eq(unit)),
            1,
            history::REVISION_BYTES + 4096,
            budget,
        )
        .await?;
        ensure!(
            orphan.is_empty(),
            "native assessment revisions have no source"
        );
        return Ok(None);
    };
    let edge: Edge = serde_json::from_str(&row.body)?;
    ensure!(
        edge.unit == unit && io::json(&edge, SOURCE_BYTES)? == row.body,
        "native assessment source encoding changed"
    );
    let completed = completed::retained(tx, reader, &edge, budget).await?;
    Ok(Some(view(tx, scope, completed, Some(row), budget).await?))
}

pub(in crate::tenancy) fn scoring(view: &View) -> ScoringInput {
    ScoringInput {
        rubric_version: rubric::RUBRIC_VERSION,
        library: rubric::library(),
        expected_revision: view
            .selected
            .as_ref()
            .map(|value| value.revision_id.clone()),
        evidence: view.completion.packet().clone(),
    }
}
fn review(view: View, budget: &OperationBudget<'_>) -> Result<Reply> {
    budget.charge(0, io::json_len(view.completion.packet(), REVIEW_BYTES)?)?;
    let scoring = scoring(&view);
    let (prefill_revision, previous) =
        history::prefill(&view.history, scoring.expected_revision.as_deref())?;
    let result = NativeRunReview {
        run_id: view.completion.edge().run.clone(),
        head: view.completion.edge().head.clone(),
        scoring,
        previous,
        prefill_revision,
        history: view.history,
    };
    io::json_len(&result, REVIEW_BYTES).map_err(|_| Error::Capacity)?;
    Ok(Reply::Review(Box::new(result)))
}

fn check_next_review(view: &View, next: &AssessmentRevision) -> Result<()> {
    use serde::{Serialize, Serializer, ser::SerializeSeq};
    struct Appended<'a> {
        previous: &'a [AssessmentRevision],
        next: &'a AssessmentRevision,
    }
    impl Serialize for Appended<'_> {
        fn serialize<S: Serializer>(&self, serializer: S) -> std::result::Result<S::Ok, S::Error> {
            let mut sequence = serializer.serialize_seq(Some(self.previous.len() + 1))?;
            for value in self.previous {
                sequence.serialize_element(value)?;
            }
            sequence.serialize_element(self.next)?;
            sequence.end()
        }
    }
    #[derive(Serialize)]
    struct Scoring<'a> {
        rubric_version: &'a str,
        library: &'a [rubric::Criterion],
        expected_revision: Option<&'a str>,
        evidence: &'a crate::core::evolution::evidence::EvidencePacket,
    }
    #[derive(Serialize)]
    struct Review<'a> {
        run_id: &'a str,
        head: &'a bitrouter_ai::core::checkpoint::DurableHead,
        scoring: Scoring<'a>,
        previous: Option<&'a rubric::RubricEvaluation>,
        prefill_revision: Option<&'a str>,
        history: Appended<'a>,
    }
    let previous = next
        .input
        .assessment
        .as_ref()
        .map(|content| serde_json::from_str::<rubric::RubricEvaluation>(&content.explanation))
        .transpose()?;
    let library = rubric::library();
    let projected = Review {
        run_id: &view.completion.edge().run,
        head: &view.completion.edge().head,
        scoring: Scoring {
            rubric_version: rubric::RUBRIC_VERSION,
            library: &library,
            expected_revision: Some(&next.revision_id),
            evidence: view.completion.packet(),
        },
        previous: previous.as_ref(),
        prefill_revision: Some(&next.revision_id),
        history: Appended {
            previous: &view.history,
            next,
        },
    };
    io::json_len(&projected, REVIEW_BYTES).map_err(|_| Error::Capacity)?;
    Ok(())
}

async fn write(
    tx: &DatabaseTransaction,
    scope: &Scope,
    provenance: &Provenance,
    view: &View,
    input: RevisionInput,
    budget: &OperationBudget<'_>,
) -> Result<(AssessmentRevision, bool)> {
    let edge = view.completion.edge();
    let id = revision_key(scope, &edge.unit, &input.submission_id)?;
    let digest = crate::storage::evolution::digest(&input)?;
    if let Some(previous) = view.history.iter().find(|value| value.revision_id == id) {
        ensure!(
            previous.input_digest == digest
                && io::json(&previous.input, history::REVISION_BYTES)?
                    == io::json(&input, history::REVISION_BYTES)?,
            Error::Rejected(ErrorCode::RevisionConflict)
        );
        return Ok((previous.clone(), false));
    }
    let expected = view
        .selected
        .as_ref()
        .map(|value| value.revision_id.clone());
    ensure!(
        input.expected_revision == expected,
        Error::Rejected(ErrorCode::RevisionConflict)
    );
    if input.assessment.is_none() {
        ensure!(
            view.selected
                .as_ref()
                .is_some_and(|value| value.input.assessment.is_some()),
            Error::Rejected(ErrorCode::RevisionConflict)
        );
    }
    let revision = AssessmentRevision {
        revision_id: id.clone(),
        input_digest: digest,
        supersedes: expected.clone(),
        input,
        selected_on_submission: true,
        selection_reason: "selected".into(),
        created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    };
    let body = io::json(&revision, history::REVISION_BYTES).map_err(|_| Error::Capacity)?;
    ensure!(
        view.history.len() < HISTORY_ROWS
            && view
                .bytes
                .checked_add(body.len())
                .is_some_and(|bytes| bytes <= HISTORY_BYTES),
        Error::Capacity
    );
    // Human edits retain room for every already accepted paid terminal,
    // sharing this same history and future-read budget before any write.
    check_next_review(view, &revision)?;
    let owed = crate::tenancy::learning::store::native::reserved_for(
        tx,
        scope.clone(),
        provenance.clone(),
        &edge.unit,
        &edge.checkpoint,
        budget,
    )
    .await?;
    if owed.rows > 0 {
        reserve_future(
            view,
            owed,
            history::Allowance {
                rows: 1,
                bytes: body.len(),
            },
            false,
            budget,
        )?;
    }
    let meta = scope.metadata(provenance)?;
    // The next review repeats this source/history read and clones the packet.
    // Charge that missing copy now. The write allowances below also cover its
    // new revision, optional first source, and head io::one/rows transfers:
    // their body plus bounded typed IDs/metadata fit the existing 4096 allowance.
    budget.charge(0, io::json_len(view.completion.packet(), REVIEW_BYTES)?)?;
    budget.charge(1, body.len().checked_add(4096).ok_or(Error::Capacity)?)?;
    budget.charge(1, 4096)?;
    let new_source = view
        .source
        .is_none()
        .then(|| io::json(edge, SOURCE_BYTES))
        .transpose()?;
    if let Some(encoded) = &new_source {
        budget.charge(1, encoded.len().checked_add(4096).ok_or(Error::Capacity)?)?;
    }
    // Complete every future-read/write reservation before the first INSERT.
    if let Some(encoded) = new_source {
        ensure!(
            records::Entity::insert(records::ActiveModel {
                record_id: Set(source_key(scope, &edge.unit)?),
                scope_id: Set(edge.unit.clone()),
                kind: Set(SOURCE_KIND.into()),
                session_key: Set(Some(edge.session.clone())),
                revision: Set(0),
                body: Set(encoded),
                installation_id: Set(meta.installation.clone()),
                tenant_id: Set(meta.tenant.clone()),
                creator_principal_id: Set(meta.creator.clone())
            })
            .exec_without_returning(tx)
            .await?
                == 1,
            "native assessment source insert failed"
        );
        budget.check()?;
    }
    ensure!(
        revisions::Entity::insert(revisions::ActiveModel {
            revision_id: Set(id.clone()),
            session_key: Set(edge.unit.clone()),
            checkpoint_id: Set(edge.checkpoint.clone()),
            created_at: Set(revision.created_at.clone()),
            revision_json: Set(body),
            installation_id: Set(meta.installation.clone()),
            tenant_id: Set(meta.tenant.clone()),
            creator_principal_id: Set(meta.creator.clone())
        })
        .exec_without_returning(tx)
        .await?
            == 1,
        "native assessment revision insert failed"
    );
    budget.check()?;
    if view.head.is_some() {
        let expected = expected.context("native assessment existing head lost identity")?;
        ensure!(
            heads::Entity::update_many()
                .filter(scope.predicate(heads::Column::InstallationId, heads::Column::TenantId))
                .filter(heads::Column::SessionKey.eq(&edge.unit))
                .filter(heads::Column::RevisionId.eq(expected))
                .col_expr(heads::Column::RevisionId, Expr::value(Some(id)))
                .exec(tx)
                .await?
                .rows_affected
                == 1,
            Error::Rejected(ErrorCode::RevisionConflict)
        );
    } else {
        ensure!(
            heads::Entity::insert(heads::ActiveModel {
                session_key: Set(edge.unit.clone()),
                revision_id: Set(Some(id)),
                installation_id: Set(meta.installation),
                tenant_id: Set(meta.tenant),
                creator_principal_id: Set(meta.creator)
            })
            .exec_without_returning(tx)
            .await?
                == 1,
            "native assessment head insert failed"
        );
    }
    budget.check()?;
    Ok((revision, true))
}

pub(super) async fn execute(
    tx: &DatabaseTransaction,
    binding: &Binding,
    retained: &crate::tenancy::resources::execution::RetainedExecutionRevision,
    input: &Input,
    budget: &OperationBudget<'_>,
) -> Result<(Reply, bool)> {
    let completed = completed::live(tx, binding, budget).await?;
    completed::check_retained(retained, completed.profile())?;
    let (scope, provenance) = Scope::for_session(binding.authority());
    let view = load(tx, &scope, completed, budget).await?;
    let requested = match input {
        Input::Review(value) => value.as_deref(),
        Input::Submit(value) => Some(value.checkpoint_id.as_str()),
        Input::Retract(value) => Some(value.checkpoint_id.as_str()),
    };
    ensure!(
        requested.is_none_or(|id| id == view.completion.edge().checkpoint),
        Error::Rejected(ErrorCode::RevisionConflict)
    );
    match input {
        Input::Review(_) => Ok((review(view, budget)?, false)),
        Input::Submit(value) => {
            let packet = view.completion.packet();
            let quality = value
                .evaluation
                .aggregate(packet)
                .map_err(|_| Error::Rejected(ErrorCode::InvalidRequest))?;
            let pipeline = scoring::measurement_contract_for(
                &packet.projection_version,
                AssessmentSource::Human,
                &value.evaluator_id,
                &value.evaluator_version,
            )?;
            let assessment = value
                .evaluation
                .assessment(packet, &pipeline)
                .map_err(|_| Error::Rejected(ErrorCode::InvalidRequest))?;
            let input = RevisionInput {
                submission_id: value.submission_id.clone(),
                checkpoint_id: value.checkpoint_id.clone(),
                expected_revision: value.expected_revision.clone(),
                source: AssessmentSource::Human,
                evaluator_id: value.evaluator_id.clone(),
                evaluator_version: value.evaluator_version.clone(),
                assessment: Some(assessment),
                reason: value.evaluation.summary.clone(),
            };
            let (revision, wrote) = write(tx, &scope, &provenance, &view, input, budget).await?;
            Ok((
                Reply::Submitted(ScoringReceipt { quality, revision }),
                wrote,
            ))
        }
        Input::Retract(input) => {
            let (revision, wrote) =
                write(tx, &scope, &provenance, &view, input.clone(), budget).await?;
            Ok((Reply::Retracted(revision), wrote))
        }
    }
}

/// Reserve the paid terminal's maximal existing1MiB revision plus its exact-job
/// validation row. Human edits must leave all already accepted obligations intact.
pub(in crate::tenancy) fn reserve_paid(
    view: &View,
    held: history::Allowance,
    adding: bool,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let added = if adding {
        history::Allowance {
            rows: 1,
            bytes: history::REVISION_BYTES,
        }
    } else {
        history::Allowance::default()
    };
    reserve_future(view, held, added, true, budget)
}
fn reserve_future(
    view: &View,
    held: history::Allowance,
    additional: history::Allowance,
    paid: bool,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let rows = view
        .history
        .len()
        .checked_add(held.rows)
        .and_then(|n| n.checked_add(additional.rows))
        .ok_or(Error::Capacity)?;
    let bytes = view
        .bytes
        .checked_add(held.bytes)
        .and_then(|n| n.checked_add(additional.bytes))
        .ok_or(Error::Capacity)?;
    ensure!(
        rows <= HISTORY_ROWS && bytes <= HISTORY_BYTES,
        Error::Capacity
    );
    // Count actual immutable evidence and rubric text without cloning. The
    // 4096 framing covers the bounded run/head IDs and actual response field
    // names; one maximal supported prefill is retained in addition to history.
    let fixed = io::json_len(
        &(
            &view.completion.edge().run,
            &view.completion.edge().head,
            rubric::RUBRIC_VERSION,
            rubric::library(),
            view.completion.packet(),
        ),
        REVIEW_BYTES,
    )?;
    let reply = fixed
        .checked_add(4096)
        .and_then(|n| n.checked_add(bytes))
        .and_then(|n| n.checked_add(rows))
        .and_then(|n| n.checked_add(history::REVISION_BYTES))
        .ok_or(Error::Capacity)?;
    ensure!(reply <= REVIEW_BYTES, Error::Capacity);
    let jobs = held
        .rows
        .checked_add(if paid { additional.rows } else { 0 })
        .ok_or(Error::Capacity)?;
    let history = held
        .bytes
        .checked_add(if paid { additional.bytes } else { 0 })
        .ok_or(Error::Capacity)?;
    // Each future Agentic row also loads its immutable paid result record. This
    // is distinct from its history bytes and must share the same read budget.
    let per_job = history::REVISION_BYTES
        .checked_add(1024 * 1024 + crate::tenancy::receipts::auxiliary::MAX_BYTES + 16 * 1024)
        .and_then(|n| n.checked_add(io::json_len(view.completion.packet(), REVIEW_BYTES).ok()?))
        .ok_or(Error::Capacity)?;
    let future = history
        .checked_add(jobs.checked_mul(per_job).ok_or(Error::Capacity)?)
        .and_then(|n| {
            n.checked_add(if view.source.is_none() {
                SOURCE_BYTES + 4096
            } else {
                0
            })
        })
        .and_then(|n| n.checked_add(4096))
        .ok_or(Error::Capacity)?;
    budget.charge(
        jobs.checked_mul(6)
            .and_then(|n| n.checked_add(2))
            .ok_or(Error::Capacity)?,
        future,
    )?;
    if paid {
        budget.charge(0, io::json_len(view.completion.packet(), REVIEW_BYTES)?)?;
    }
    Ok(())
}

/// The caller already proved the complete source and owns the same H transaction.
pub(in crate::tenancy) async fn retain_source(
    tx: &DatabaseTransaction,
    scope: &Scope,
    provenance: &Provenance,
    completion: &Completion,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let edge = completion.edge();
    if let Some(row) = source(tx, scope, &edge.unit, budget).await? {
        metadata(
            scope,
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
            completion.principal(),
        )?;
        let saved: Edge = serde_json::from_str(&row.body)?;
        ensure!(
            saved.matches(edge)
                && row.session_key.as_deref() == Some(edge.session.as_str())
                && io::json(&saved, SOURCE_BYTES)? == row.body,
            "native retained source changed"
        );
        return Ok(());
    }
    let meta = scope.metadata(provenance)?;
    let body = io::json(edge, SOURCE_BYTES)?;
    budget.charge(1, body.len().checked_add(4096).ok_or(Error::Capacity)?)?;
    ensure!(
        records::Entity::insert(records::ActiveModel {
            record_id: Set(source_key(scope, &edge.unit)?),
            scope_id: Set(edge.unit.clone()),
            kind: Set(SOURCE_KIND.into()),
            session_key: Set(Some(edge.session.clone())),
            revision: Set(0),
            body: Set(body),
            installation_id: Set(meta.installation),
            tenant_id: Set(meta.tenant),
            creator_principal_id: Set(meta.creator),
        })
        .exec_without_returning(tx)
        .await?
            == 1,
        "native paid source insert failed"
    );
    budget.check()?;
    Ok(())
}
