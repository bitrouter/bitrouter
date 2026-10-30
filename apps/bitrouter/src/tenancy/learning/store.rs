//! Borrowed transactions only. Logical acceptance is distinct from model attempts.
pub(in crate::tenancy) mod capture_terminal;
#[cfg(test)]
mod history_tests;
pub(in crate::tenancy::learning) mod namespace;
pub(in crate::tenancy) mod native;
pub(in crate::tenancy) mod recovery;
pub(in crate::tenancy) mod stopped;
use super::{
    entities::{heads, revisions},
    history,
};
use crate::{
    acp_trajectory::checkpoint::{
        owned::{Summary, assessment},
        types::{AssessmentRevision, AssessmentSource, RevisionInput},
    },
    core::evolution::{judge, rubric, scoring::ScoringInput},
    harness::trajectory::owned::{Association, entities::records, io},
    storage::{
        operation_budget::OperationBudget,
        scope::{KeyDomain, MetadataRef},
    },
    tenancy::capture::Binding,
};
use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    configuration::ResourceRef,
    identity::{ExecutionId, RevisionId},
    operation::ErrorCode,
};
use namespace::Namespace;
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseTransaction, EntityTrait, QueryFilter, QuerySelect, Set,
    sea_query::{Alias, Expr},
};
use serde::{Deserialize, Serialize};

const JOB: &str = "owned_judge_job";
const ACTIVE: &str = "owned_judge_active_v3";
const BUDGET: &str = "owned_judge_budget";
pub(super) const PENDING: &str = "owned_judge_pending";
const RECORD_BYTES: usize = 1024 * 1024;

pub(in crate::tenancy) struct Prepared {
    pub id: String,
    pub session: String,
    pub source: PreparedSource,
    pub revision: RevisionId,
    pub route: ResourceRef,
    pub input: ScoringInput,
    pub input_digest: String,
    pub maximum: u32,
    pub accepted_at: String,
}
pub(in crate::tenancy) enum PreparedSource {
    Capture(Summary),
    Native(Box<super::native::PreparedSource>),
}
impl Prepared {
    fn capture_summary(&self) -> Result<&Summary> {
        match &self.source {
            PreparedSource::Capture(summary) => Ok(summary),
            PreparedSource::Native(_) => anyhow::bail!("native paid source is not ACP"),
        }
    }
    pub(super) fn checkpoint(&self) -> &str {
        &self.input.evidence.checkpoint_id
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub(in crate::tenancy) enum Status {
    Accepted,
    Completed,
    Unselected,
    Failed,
    Interrupted,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Record {
    version: u32,
    job: String,
    session: String,
    configuration: RevisionId,
    route: ResourceRef,
    checkpoint: String,
    prefix_digest: String,
    input_digest: String,
    expected_revision: Option<String>,
    execution: ExecutionId,
    status: Status,
    accepted_at: String,
    output: Option<judge::JudgeOutput>,
    assessment: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    history_reservation: Option<HistoryReservation>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    recovery: Option<super::recovery::Retained>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    native_evidence: Option<crate::core::evolution::evidence::EvidencePacket>,
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct HistoryReservation {
    checkpoint: String,
    rows: u8,
    bytes: usize,
    state: ReservationState,
}
#[derive(Serialize, Deserialize)]
#[serde(tag = "state", rename_all = "snake_case", deny_unknown_fields)]
enum ReservationState {
    Held,
    Consumed { actual_bytes: usize },
    ReleasedWithoutRevision,
}
fn valid_record(value: &Record) -> bool {
    if (value.version == 4) != value.native_evidence.is_some() {
        return false;
    }
    if let Some(packet) = &value.native_evidence
        && (packet.projection_version != crate::core::evolution::evidence::NATIVE_EVIDENCE_VERSION
            || packet.checkpoint_id != value.checkpoint
            || packet.prefix_digest != value.prefix_digest
            || !packet.gaps.is_empty()
            || packet.items.is_empty()
            || packet.items.len() > 4096)
    {
        return false;
    }
    match (&value.recovery, value.version) {
        (None, 1 | 2) => {}
        (Some(recovery), 3 | 4) => {
            if (value.version == 4) != recovery.seed.native.is_some() {
                return false;
            }
            let seed = &recovery.seed;
            if seed.validate().is_err()
                || seed.job != value.job
                || seed.session != value.session
                || seed.execution != value.execution
                || seed.configuration != value.configuration
                || seed.evaluator != value.route
                || seed.checkpoint != value.checkpoint
                || seed.prefix_digest != value.prefix_digest
                || seed.input_digest != value.input_digest
            {
                return false;
            }
            if recovery
                .stopped
                .as_ref()
                .is_some_and(|stopped| stopped.validate(seed).is_err())
            {
                return false;
            }
            if let Some(observed) = &recovery.observed {
                if observed.receipt_digest.len() != 64
                    || !observed
                        .receipt_digest
                        .bytes()
                        .all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
                    || value.output.as_ref().is_none_or(|output| {
                        !crate::storage::evolution::digest(output)
                            .is_ok_and(|digest| digest == observed.output_digest)
                    })
                {
                    return false;
                }
            } else if value.output.is_some() {
                return false;
            }
        }
        _ => return false,
    }
    match (value.version, &value.history_reservation) {
        (1, None) => matches!(
            value.status,
            Status::Completed | Status::Unselected | Status::Failed
        ),
        (2..=4, Some(reservation)) => {
            reservation.checkpoint == value.checkpoint
                && reservation.rows == 1
                && reservation.bytes == history::REVISION_BYTES
                && match (&value.status, &reservation.state) {
                    (Status::Accepted, ReservationState::Held) => {
                        (value.version >= 3 || value.output.is_none()) && value.assessment.is_none()
                    }
                    (
                        Status::Completed | Status::Unselected,
                        ReservationState::Consumed { actual_bytes },
                    ) => {
                        *actual_bytes > 0
                            && *actual_bytes <= reservation.bytes
                            && value.output.is_some()
                            && value.assessment.is_some()
                    }
                    (
                        Status::Failed | Status::Interrupted,
                        ReservationState::ReleasedWithoutRevision,
                    ) => value.output.is_none() && value.assessment.is_none(),
                    _ => false,
                }
        }
        _ => false,
    }
}
fn record_revision(value: &Record) -> i64 {
    i64::from(
        value
            .recovery
            .as_ref()
            .is_some_and(|r| r.observed.is_some()),
    ) + i64::from(value.recovery.as_ref().is_some_and(|r| r.stopped.is_some()))
        + i64::from(
            value
                .recovery
                .as_ref()
                .is_some_and(|r| r.native_cleanup.is_some()),
        )
        + i64::from(value.status != Status::Accepted)
}
fn record_kind(value: &Record) -> &'static str {
    if value.version >= 3 && value.status == Status::Accepted {
        ACTIVE
    } else {
        JOB
    }
}
fn validate_live_seed(value: &Record, binding: &Binding) -> Result<()> {
    if let Some(recovery) = &value.recovery {
        ensure!(
            recovery.seed.descriptor.as_ref() == Some(binding.association().descriptor())
                && recovery.seed.connection.as_deref() == Some(binding.association().connection()),
            "paid seed capture binding mismatch"
        );
        let origin = binding
            .service()
            .startup
            .as_ref()
            .context("paid original acquisition absent")?
            .stamp(&binding.service().cleanup)?;
        ensure!(
            origin == recovery.seed.origin,
            "paid original acquisition mismatch"
        );
    }
    Ok(())
}
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Counter {
    version: u32,
    configuration: RevisionId,
    accepted: u32,
}

fn scope(association: &impl Namespace) -> sea_orm::Condition {
    association.scope().predicate(
        (
            Alias::new("checkpoint_evolution"),
            Alias::new("installation_id"),
        ),
        (Alias::new("checkpoint_evolution"), Alias::new("tenant_id")),
    )
}
async fn record(
    tx: &DatabaseTransaction,
    a: &impl Namespace,
    kind: &str,
    id: &str,
    budget: &OperationBudget<'_>,
) -> Result<Option<records::Model>> {
    let row = io::one(
        tx,
        records::Entity::find_by_id(id)
            .filter(scope(a))
            .filter(if kind == JOB {
                records::Column::Kind.is_in([JOB, ACTIVE])
            } else {
                records::Column::Kind.eq(kind)
            })
            .lock_exclusive(),
        RECORD_BYTES + 4096,
        budget,
    )
    .await?;
    if let Some(row) = &row {
        a.scope().validate_metadata(MetadataRef {
            installation: row.installation_id.as_deref(),
            tenant: row.tenant_id.as_deref(),
            creator: row.creator_principal_id.as_deref(),
        })?;
        ensure!(
            row.revision >= 0 && row.scope_id == a.key(KeyDomain::LearningPending, &[])?,
            "learning record scope mismatch"
        );
    } else {
        // Scoped absence cannot authorize replacing a foreign/Legacy occupant.
        budget.charge(1, 128)?;
        ensure!(
            records::Entity::find_by_id(id)
                .select_only()
                .column_as(Expr::val(1), "occupied")
                .into_tuple::<i32>()
                .one(tx)
                .await?
                .is_none(),
            "learning physical key collision"
        );
        budget.check()?;
    }
    Ok(row)
}
async fn insert(
    tx: &DatabaseTransaction,
    a: &impl Namespace,
    id: &str,
    kind: &str,
    session: Option<&str>,
    body: &impl Serialize,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let body = io::json(body, RECORD_BYTES)?;
    budget.charge(1, body.len() + 1024)?;
    let meta = a.metadata()?;
    ensure!(
        records::Entity::insert(records::ActiveModel {
            record_id: Set(id.to_owned()),
            scope_id: Set(a.key(KeyDomain::LearningPending, &[])?),
            kind: Set(kind.to_owned()),
            session_key: Set(session.map(str::to_owned)),
            revision: Set(0),
            body: Set(body),
            installation_id: Set(meta.installation),
            tenant_id: Set(meta.tenant),
            creator_principal_id: Set(meta.creator),
        })
        .exec_without_returning(tx)
        .await?
            == 1,
        "learning record insert failed"
    );
    budget.check()?;
    Ok(())
}
async fn save(
    tx: &DatabaseTransaction,
    a: &impl Namespace,
    row: &records::Model,
    value: &impl Serialize,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    save_as(tx, a, row, &row.kind, value, budget).await
}
async fn save_as(
    tx: &DatabaseTransaction,
    a: &impl Namespace,
    row: &records::Model,
    kind: &str,
    value: &impl Serialize,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let body = io::json(value, RECORD_BYTES)?;
    budget.charge(1, body.len() + 1024)?;
    let revision = row
        .revision
        .checked_add(1)
        .context("learning revision overflow")?;
    let changed = records::Entity::update_many()
        .filter(scope(a))
        .filter(records::Column::RecordId.eq(&row.record_id))
        .filter(records::Column::Kind.eq(&row.kind))
        .filter(records::Column::Revision.eq(row.revision))
        .col_expr(records::Column::Kind, Expr::value(kind))
        .col_expr(records::Column::Revision, Expr::value(revision))
        .col_expr(records::Column::Body, Expr::value(body))
        .exec(tx)
        .await?;
    ensure!(changed.rows_affected == 1, "learning record CAS failed");
    budget.check()?;
    Ok(())
}
pub(in crate::tenancy) async fn head(
    tx: &DatabaseTransaction,
    a: &Association,
    session: &str,
    budget: &OperationBudget<'_>,
) -> Result<Option<heads::Model>> {
    let row = io::one(
        tx,
        heads::Entity::find_by_id(session)
            .filter(a.predicate("acp_effective_assessments"))
            .lock_exclusive(),
        4096,
        budget,
    )
    .await?;
    if let Some(row) = &row {
        a.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
    } else {
        // Expected absence must not hide a foreign/Legacy occupant of this
        // already-derived physical key. Inspect existence only, no foreign body.
        budget.charge(1, 128)?;
        let query = sea_orm::sea_query::Query::select()
            .expr(Expr::val(1))
            .from(Alias::new("acp_effective_assessments"))
            .and_where(Expr::col(Alias::new("session_key")).eq(session))
            .limit(1)
            .to_owned();
        ensure!(
            tx.query_one(tx.get_database_backend().build(&query))
                .await?
                .is_none(),
            "assessment head namespace conflict"
        );
        budget.check()?;
    }
    Ok(row)
}

pub(in crate::tenancy) async fn prepare(
    tx: &DatabaseTransaction,
    binding: &Binding,
    native: &str,
    summary: &Summary,
    budget: &OperationBudget<'_>,
) -> Result<Prepared> {
    let profile = binding.assessment().context("assessment binding absent")?;
    let (session, evidence) =
        assessment::evidence_in(tx, binding.association(), native, summary, budget).await?;
    let expected = head(tx, binding.association(), &session, budget)
        .await?
        .and_then(|head| head.revision_id);
    let input = ScoringInput {
        rubric_version: rubric::RUBRIC_VERSION,
        library: rubric::library(),
        expected_revision: expected,
        evidence,
    };
    io::json(&input, 12 * 1024 * 1024)?;
    let input_digest = judge::input_digest(&input, profile.route.resource_id.as_str())?;
    let route = io::json(&profile.route, 4096)?;
    let id = binding.association().key(
        KeyDomain::LearningJob,
        &[
            binding.authority().revision().as_str(),
            &session,
            &summary.checkpoint_id,
            &route,
            &input_digest,
            judge::JUDGE_VERSION,
        ],
    )?;
    Ok(Prepared {
        id,
        session,
        source: PreparedSource::Capture(summary.clone()),
        revision: binding.authority().revision().clone(),
        route: profile.route.clone(),
        input,
        input_digest,
        maximum: profile.maximum,
        accepted_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
    })
}

fn same(record: &Record, p: &Prepared) -> bool {
    valid_record(record)
        && record.job == p.id
        && record.session == p.session
        && record.configuration == p.revision
        && record.route == p.route
        && record.checkpoint == p.input.evidence.checkpoint_id
        && record.prefix_digest == p.input.evidence.prefix_digest
        && record.input_digest == p.input_digest
}

/// Duplicate returns its original status and never increments another budget.
pub(in crate::tenancy) async fn accept(
    tx: &DatabaseTransaction,
    binding: &super::source::Binding,
    p: &Prepared,
    execution: &ExecutionId,
    recovery: Option<&super::recovery::Seed>,
    budget: &OperationBudget<'_>,
) -> Result<Option<Status>> {
    let binding = match binding {
        super::source::Binding::Capture(binding) => binding,
        super::source::Binding::Native(native) => {
            return super::native::accept(tx, native, p, execution, recovery, budget).await;
        }
    };
    let a = binding.association();
    let profile = binding.assessment().context("assessment binding absent")?;
    ensure!(
        p.route == profile.route
            && p.revision == *binding.authority().revision()
            && p.maximum == profile.maximum,
        "prepared assessment binding mismatch"
    );
    a.connection_in(tx, budget).await?;
    if let Some(row) = record(tx, a, JOB, &p.id, budget).await? {
        a.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        let value: Record = serde_json::from_str(&row.body)?;
        ensure!(
            same(&value, p)
                && row.session_key.as_deref() == Some(&p.session)
                && row.revision == record_revision(&value)
                && row.kind == record_kind(&value)
                && io::json(&value, RECORD_BYTES)? == row.body,
            "logical assessment identity conflict"
        );
        validate_live_seed(&value, binding)?;
        return Ok(Some(value.status));
    }
    let current = head(tx, a, &p.session, budget)
        .await?
        .and_then(|h| h.revision_id);
    ensure!(
        current == p.input.expected_revision,
        crate::storage::operation_budget::Error::Rejected(ErrorCode::RevisionConflict)
    );
    let retained = history::load(
        tx,
        a,
        &p.session,
        &p.capture_summary()?.checkpoint_id,
        budget,
    )
    .await?;
    let owed = reserved_in(
        tx,
        a,
        &p.session,
        &p.capture_summary()?.checkpoint_id,
        budget,
    )
    .await?;
    history::admit(
        p.capture_summary()?,
        &p.input,
        &retained,
        owed,
        history::Allowance {
            rows: 1,
            bytes: history::REVISION_BYTES,
        },
    )?;
    let key = a.key(KeyDomain::LearningBudget, &[p.revision.as_str()])?;
    let existing = record(tx, a, BUDGET, &key, budget).await?;
    let mut counter: Counter = match &existing {
        Some(row) => serde_json::from_str(&row.body)?,
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
            && existing.as_ref().is_none_or(|row| {
                row.session_key.is_none()
                    && row.revision.checked_add(1) == Some(i64::from(counter.accepted))
            }),
        "revision learning budget mismatch"
    );
    ensure!(
        counter.accepted < p.maximum,
        crate::storage::operation_budget::Error::Rejected(ErrorCode::CapacityExceeded)
    );
    counter.accepted = counter
        .accepted
        .checked_add(1)
        .context("learning budget overflow")?;
    match &existing {
        Some(row) => save(tx, a, row, &counter, budget).await?,
        None => insert(tx, a, &key, BUDGET, None, &counter, budget).await?,
    }
    if let Some(seed) = recovery {
        seed.validate()?;
        ensure!(seed.matches(p, execution), "paid admission seed mismatch");
    }
    let value = Record {
        version: if recovery.is_some() { 3 } else { 2 },
        job: p.id.clone(),
        session: p.session.clone(),
        configuration: p.revision.clone(),
        route: p.route.clone(),
        checkpoint: p.input.evidence.checkpoint_id.clone(),
        prefix_digest: p.input.evidence.prefix_digest.clone(),
        input_digest: p.input_digest.clone(),
        expected_revision: p.input.expected_revision.clone(),
        execution: execution.clone(),
        status: Status::Accepted,
        accepted_at: p.accepted_at.clone(),
        output: None,
        assessment: None,
        native_evidence: None,
        recovery: recovery.map(|seed| super::recovery::Retained {
            seed: seed.clone(),
            observed: None,
            stopped: None,
            native_cleanup: None,
        }),
        history_reservation: Some(HistoryReservation {
            checkpoint: p.checkpoint().to_owned(),
            rows: 1,
            bytes: history::REVISION_BYTES,
            state: ReservationState::Held,
        }),
    };
    insert(
        tx,
        a,
        &p.id,
        record_kind(&value),
        Some(&p.session),
        &value,
        budget,
    )
    .await?;
    let pending = a.key(KeyDomain::LearningPending, &[&p.id])?;
    insert(tx, a, &pending, PENDING, Some(&p.session), &value, budget).await?;
    Ok(None)
}

/// Retain known output even when current authority forbids selecting it. This
/// is owner-bound finalization, never a new model attempt or policy activation.
pub(in crate::tenancy) async fn finish(
    tx: &DatabaseTransaction,
    binding: &super::source::Binding,
    p: &Prepared,
    execution: &ExecutionId,
    output: Option<&judge::JudgeOutput>,
    select: bool,
    budget: &OperationBudget<'_>,
) -> Result<Status> {
    let binding = match binding {
        super::source::Binding::Capture(binding) => binding,
        super::source::Binding::Native(native) => {
            return super::native::finish(tx, native, p, execution, output, select, budget).await;
        }
    };
    let a = binding.association();
    let profile = binding.assessment().context("assessment binding absent")?;
    ensure!(
        p.route == profile.route
            && p.revision == *binding.authority().revision()
            && p.maximum == profile.maximum,
        "prepared assessment binding mismatch"
    );
    a.connection_in(tx, budget).await?;
    let row = record(tx, a, JOB, &p.id, budget)
        .await?
        .context("accepted assessment absent")?;
    a.validate(
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
    )?;
    let mut value: Record = serde_json::from_str(&row.body)?;
    ensure!(
        same(&value, p)
            && row.revision == record_revision(&value)
            && row.kind == record_kind(&value)
            && io::json(&value, RECORD_BYTES)? == row.body
            && value.execution == *execution
            && value.expected_revision == p.input.expected_revision
            && value.accepted_at == p.accepted_at
            && row.session_key.as_deref() == Some(&p.session),
        "assessment completion identity mismatch"
    );
    validate_live_seed(&value, binding)?;
    if value.status != Status::Accepted {
        ensure!(
            io::json(&value.output, RECORD_BYTES)? == io::json(&output, RECORD_BYTES)?,
            "assessment completion replay mismatch"
        );
        return Ok(value.status);
    }
    let pending_key = a.key(KeyDomain::LearningPending, &[&p.id])?;
    let pending = record(tx, a, PENDING, &pending_key, budget)
        .await?
        .context("assessment pending index absent")?;
    a.validate(
        pending.installation_id.as_deref(),
        pending.tenant_id.as_deref(),
        pending.creator_principal_id.as_deref(),
    )?;
    ensure!(
        pending.body == row.body
            && pending.session_key == row.session_key
            && pending.revision == row.revision,
        "assessment pending index mismatch"
    );
    if let Some(recovery) = &value.recovery {
        ensure!(
            io::json(&value.output, RECORD_BYTES)? == io::json(&output, RECORD_BYTES)?,
            "paid output differs from acknowledged observation"
        );
        if let Some(observed) = &recovery.observed {
            verify_receipt(tx, &recovery.seed, &observed.receipt_digest, budget).await?;
        }
    }
    value.status = Status::Failed;
    let mut terminal_reservation = ReservationState::ReleasedWithoutRevision;
    if let Some(output) = output {
        ensure!(
            output.request_id == execution.as_str()
                && output.model == p.route.resource_id.as_str()
                && output.judge_version == judge::JUDGE_VERSION
                && output.input_digest == p.input_digest,
            "judge output identity mismatch"
        );
        output.evaluation.aggregate(&p.input.evidence)?;
        let watermark = assessment::validate_in(
            tx,
            a,
            &p.session,
            &value.checkpoint,
            Some(&value.prefix_digest),
            budget,
        )
        .await?;
        let current = head(tx, a, &p.session, budget).await?;
        let current_id = current.as_ref().and_then(|head| head.revision_id.clone());
        let mut selected = select && current_id == value.expected_revision;
        let mut reason = if !select {
            "authority_changed"
        } else if !selected {
            "head_changed"
        } else {
            "selected"
        };
        if selected && let Some(id) = &current_id {
            let previous = io::one(
                tx,
                revisions::Entity::find_by_id(id).filter(a.predicate("acp_assessment_revisions")),
                RECORD_BYTES + 4096,
                budget,
            )
            .await?
            .context("assessment head revision absent")?;
            a.validate(
                previous.installation_id.as_deref(),
                previous.tenant_id.as_deref(),
                previous.creator_principal_id.as_deref(),
            )?;
            let previous_value: AssessmentRevision = serde_json::from_str(&previous.revision_json)?;
            ensure!(
                previous.session_key == p.session
                    && previous.revision_id == previous_value.revision_id
                    && previous.checkpoint_id == previous_value.input.checkpoint_id
                    && previous.created_at == previous_value.created_at
                    && previous_value.selected_on_submission
                    && previous_value.input_digest
                        == crate::storage::evolution::digest(&previous_value.input)?,
                "assessment head revision mismatch"
            );
            let previous_watermark =
                assessment::validate_in(tx, a, &p.session, &previous.checkpoint_id, None, budget)
                    .await?;
            if watermark < previous_watermark {
                selected = false;
                reason = "historical_checkpoint";
            } else if previous.checkpoint_id == value.checkpoint
                && previous_value.input.source == AssessmentSource::Human
            {
                selected = false;
                reason = "human_revision_preserved";
            }
        }
        let evaluator = io::json(&p.route, 4096)?;
        let pipeline = crate::core::evolution::scoring::measurement_contract(
            AssessmentSource::Agentic,
            &evaluator,
            judge::JUDGE_VERSION,
        )?;
        let input = RevisionInput {
            submission_id: p.id.clone(),
            checkpoint_id: value.checkpoint.clone(),
            expected_revision: value.expected_revision.clone(),
            source: AssessmentSource::Agentic,
            evaluator_id: evaluator,
            evaluator_version: judge::JUDGE_VERSION.into(),
            assessment: Some(output.evaluation.assessment(&p.input.evidence, &pipeline)?),
            reason: output.evaluation.summary.clone(),
        };
        let revision_id = a.key(KeyDomain::AssessmentRevision, &[&p.session, &p.id])?;
        let revision = AssessmentRevision {
            revision_id: revision_id.clone(),
            input_digest: crate::storage::evolution::digest(&input)?,
            input,
            supersedes: if selected { current_id.clone() } else { None },
            selected_on_submission: selected,
            selection_reason: reason.into(),
            created_at: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true),
        };
        let body = io::json(&revision, RECORD_BYTES)?;
        // Admission already reserved this maximal row. Other writers cannot
        // consume it; completion never makes a new history admission decision.
        terminal_reservation = ReservationState::Consumed {
            actual_bytes: body.len(),
        };
        budget.charge(1, body.len() + 1024)?;
        let meta = a.metadata()?;
        ensure!(
            revisions::Entity::insert(revisions::ActiveModel {
                revision_id: Set(revision_id.clone()),
                session_key: Set(p.session.clone()),
                checkpoint_id: Set(value.checkpoint.clone()),
                created_at: Set(revision.created_at),
                revision_json: Set(body),
                installation_id: Set(meta.installation.clone()),
                tenant_id: Set(meta.tenant.clone()),
                creator_principal_id: Set(meta.creator.clone()),
            })
            .exec_without_returning(tx)
            .await?
                == 1,
            "assessment revision insert failed"
        );
        budget.check()?;
        if selected {
            budget.charge(1, 4096)?;
            if current.is_some() {
                let expected = match current_id {
                    Some(id) => heads::Column::RevisionId.eq(id),
                    None => heads::Column::RevisionId.is_null(),
                };
                ensure!(
                    heads::Entity::update_many()
                        .filter(a.predicate("acp_effective_assessments"))
                        .filter(heads::Column::SessionKey.eq(&p.session))
                        .filter(expected)
                        .col_expr(
                            heads::Column::RevisionId,
                            Expr::value(Some(revision_id.clone()))
                        )
                        .exec(tx)
                        .await?
                        .rows_affected
                        == 1,
                    "assessment head CAS failed"
                );
            } else {
                ensure!(
                    heads::Entity::insert(heads::ActiveModel {
                        session_key: Set(p.session.clone()),
                        revision_id: Set(Some(revision_id.clone())),
                        installation_id: Set(meta.installation),
                        tenant_id: Set(meta.tenant),
                        creator_principal_id: Set(meta.creator),
                    })
                    .exec_without_returning(tx)
                    .await?
                        == 1,
                    "assessment head insert conflict"
                );
            }
            budget.check()?;
        }
        value.status = if selected {
            Status::Completed
        } else {
            Status::Unselected
        };
        value.assessment = Some(revision_id);
        value.output = Some(output.clone());
    }
    value
        .history_reservation
        .as_mut()
        .context("accepted history reservation absent")?
        .state = terminal_reservation;
    ensure!(
        valid_record(&value),
        "terminal history reservation mismatch"
    );
    save_as(tx, a, &row, JOB, &value, budget).await?;
    budget.charge(1, 4096)?;
    ensure!(
        records::Entity::delete_many()
            .filter(scope(a))
            .filter(records::Column::RecordId.eq(pending_key))
            .filter(records::Column::Kind.eq(PENDING))
            .filter(records::Column::Revision.eq(pending.revision))
            .exec(tx)
            .await?
            .rows_affected
            == 1,
        "assessment pending index CAS failed"
    );
    budget.check()?;
    Ok(value.status)
}

/// All pending accepted obligations of this exact owned session, not host state.
pub(in crate::tenancy) async fn reserved_in(
    tx: &DatabaseTransaction,
    a: &Association,
    session: &str,
    checkpoint: &str,
    budget: &OperationBudget<'_>,
) -> Result<history::Allowance> {
    let partition = a.key(KeyDomain::LearningPending, &[])?;
    let rows = io::rows(
        tx,
        records::Entity::find()
            .filter(a.predicate("checkpoint_evolution"))
            .filter(records::Column::ScopeId.eq(&partition))
            .filter(records::Column::Kind.eq(PENDING))
            .filter(records::Column::SessionKey.eq(session))
            .lock_exclusive(),
        8,
        8 * (RECORD_BYTES + 4096),
        budget,
    )
    .await?;
    let mut total = history::Allowance::default();
    for row in rows {
        a.validate(
            row.installation_id.as_deref(),
            row.tenant_id.as_deref(),
            row.creator_principal_id.as_deref(),
        )?;
        let value: Record = serde_json::from_str(&row.body)?;
        ensure!(
            valid_record(&value)
                && matches!(value.version, 2 | 3)
                && value.status == Status::Accepted
                && value.session == session
                && row.revision == record_revision(&value)
                && row.record_id == a.key(KeyDomain::LearningPending, &[&value.job])?
                && io::json(&value, RECORD_BYTES)? == row.body,
            "pending history reservation mismatch"
        );
        let job = record(tx, a, JOB, &value.job, budget)
            .await?
            .context("reserved job is absent")?;
        a.validate(
            job.installation_id.as_deref(),
            job.tenant_id.as_deref(),
            job.creator_principal_id.as_deref(),
        )?;
        ensure!(
            job.body == row.body
                && job.session_key == row.session_key
                && job.revision == row.revision
                && job.kind == record_kind(&value),
            "reserved job/pending mirror mismatch"
        );
        if value.checkpoint == checkpoint {
            let reservation = value
                .history_reservation
                .context("history reservation absent")?;
            total.rows = total
                .rows
                .checked_add(usize::from(reservation.rows))
                .ok_or(crate::storage::operation_budget::Error::Capacity)?;
            total.bytes = total
                .bytes
                .checked_add(reservation.bytes)
                .ok_or(crate::storage::operation_budget::Error::Capacity)?;
        }
    }
    Ok(total)
}

async fn verify_receipt(
    tx: &DatabaseTransaction,
    seed: &super::recovery::Seed,
    digest: &str,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    // Receipt and auxiliary stores preflight their own numeric lengths. Charge
    // both maxima before either body may be transferred into this operation.
    budget.charge(
        4,
        1024 * 1024 + crate::tenancy::receipts::auxiliary::MAX_BYTES + 8192,
    )?;
    let candidate = crate::tenancy::receipts::store::lookup_candidate(
        tx,
        &seed.installation,
        &seed.tenant,
        &seed.execution,
    )
    .await?
    .context("paid observed receipt absent")?;
    budget.check()?;
    seed.receipt(&candidate)?;
    ensure!(
        candidate.learning_digest() == digest,
        "paid observed receipt changed"
    );
    Ok(())
}

/// Store the actual producer's parsed answer only after exact035 durability.
/// This never selects a head or derives authority from stored identity fields.
pub(in crate::tenancy) async fn observe(
    tx: &DatabaseTransaction,
    binding: &super::source::Binding,
    p: &Prepared,
    execution: &ExecutionId,
    observation: &super::owner::Observation,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    let binding = match binding {
        super::source::Binding::Capture(binding) => binding,
        super::source::Binding::Native(native) => {
            return super::native::observe(tx, native, p, execution, observation, budget).await;
        }
    };
    let a = binding.association();
    a.connection_in(tx, budget).await?;
    let row = record(tx, a, JOB, &p.id, budget)
        .await?
        .context("accepted paid job absent")?;
    a.validate(
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
    )?;
    let mut value: Record = serde_json::from_str(&row.body)?;
    ensure!(
        same(&value, p)
            && value.version == 3
            && value.execution == *execution
            && value.status == Status::Accepted
            && row.kind == ACTIVE
            && row.session_key.as_deref() == Some(p.session.as_str())
            && row.revision == record_revision(&value)
            && io::json(&value, RECORD_BYTES)? == row.body,
        "paid observation identity mismatch"
    );
    validate_live_seed(&value, binding)?;
    let pending_key = a.key(KeyDomain::LearningPending, &[&p.id])?;
    let pending = record(tx, a, PENDING, &pending_key, budget)
        .await?
        .context("paid observation pending absent")?;
    a.validate(
        pending.installation_id.as_deref(),
        pending.tenant_id.as_deref(),
        pending.creator_principal_id.as_deref(),
    )?;
    ensure!(
        pending.body == row.body
            && pending.session_key == row.session_key
            && pending.revision == row.revision,
        "paid observation pending mismatch"
    );
    let output = &observation.output;
    io::json_len(output, 512 * 1024)?;
    ensure!(
        output.request_id == execution.as_str()
            && output.model == p.route.resource_id.as_str()
            && output.judge_version == judge::JUDGE_VERSION
            && output.input_digest == p.input_digest,
        "paid observed output identity mismatch"
    );
    output.evaluation.aggregate(&p.input.evidence)?;
    let recovery = value
        .recovery
        .as_mut()
        .context("paid recovery provenance absent")?;
    recovery.seed.receipt(&observation.terminal)?;
    let digest = observation.terminal.learning_digest();
    verify_receipt(tx, &recovery.seed, digest, budget).await?;
    let output_digest = crate::storage::evolution::digest(output)?;
    if let Some(saved) = &recovery.observed {
        ensure!(
            saved.receipt_digest == digest
                && saved.output_digest == output_digest
                && io::json(&value.output, RECORD_BYTES)? == io::json(&Some(output), RECORD_BYTES)?,
            "paid observation replay mismatch"
        );
        return Ok(());
    }
    recovery.observed = Some(super::recovery::Observed {
        receipt_digest: digest.to_owned(),
        output_digest,
    });
    value.output = Some(output.clone());
    ensure!(valid_record(&value), "paid observed record invalid");
    save(tx, a, &row, &value, budget).await?;
    save(tx, a, &pending, &value, budget).await?;
    Ok(())
}

/// Owner-bound cleanup. The unconstructible caller token comes from real joins,
/// not State.closed, a disconnected callback or a stored descriptor.
pub(in crate::tenancy) async fn capture_stopped(
    tx: &DatabaseTransaction,
    proof: &crate::harness::acp::controlled::CleanupProof,
    budget: &OperationBudget<'_>,
) -> Result<()> {
    use crate::harness::trajectory::owned::{entities::connections, protocol::LocalCleanup};
    let binding = proof.binding();
    let a = binding.association();
    let origin = binding
        .service()
        .startup
        .as_ref()
        .context("actual capture origin absent")?
        .stamp(&binding.service().cleanup)?;
    let (row, mut state) = a.connection_in(tx, budget).await?;
    ensure!(
        state.closed,
        "capture producer cleanup precedes its final Port settlement"
    );
    let existing = state.local_cleanup.take();
    let cleanup = LocalCleanup {
        version: 3,
        origin_json: io::json(&origin, 256)?,
        head: state.head,
        metadata_digest: crate::storage::evolution::digest(&state)?,
    };
    if let Some(existing) = existing {
        ensure!(existing == cleanup, "capture cleanup provenance conflicts");
        return Ok(());
    }
    state.local_cleanup = Some(cleanup);
    state.validate(a, &row)?;
    let body = io::json(
        &state,
        crate::harness::trajectory::owned::MAX_METADATA_BYTES,
    )?;
    budget.charge(1, body.len() + 4096)?;
    ensure!(
        connections::Entity::update_many()
            .filter(a.predicate("acp_capture_connections"))
            .filter(connections::Column::ConnectionId.eq(a.connection()))
            .filter(connections::Column::Head.eq(row.head))
            .filter(connections::Column::MetadataJson.eq(row.metadata_json))
            .col_expr(connections::Column::MetadataJson, Expr::value(body))
            .exec(tx)
            .await?
            .rows_affected
            == 1,
        "capture local cleanup CAS failed"
    );
    budget.check()?;
    Ok(())
}
