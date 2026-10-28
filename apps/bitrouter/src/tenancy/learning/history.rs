//! Shared history admission for actual Human writes and accepted paid terminals.
use super::entities::revisions;
use crate::{
    acp_trajectory::checkpoint::{
        owned::Summary,
        types::{AssessmentRevision, AssessmentSource},
    },
    core::evolution::{rubric::RubricEvaluation, scoring::ScoringInput},
    harness::{
        acp::controlled::CheckpointReview,
        trajectory::owned::{Association, io},
    },
    storage::{
        operation_budget::{Error, OperationBudget},
        scope::KeyDomain,
    },
};
use anyhow::{Context, Result, ensure};
use sea_orm::sea_query::{Alias, Expr, Query};
use sea_orm::{
    ColumnTrait, ConnectionTrait, DatabaseBackend, DatabaseTransaction, EntityTrait, QueryFilter,
    QueryOrder, QuerySelect, QueryTrait, Select,
};
use serde::Serialize;

pub(in crate::tenancy) const REVISION_BYTES: usize = 1024 * 1024;
const HISTORY_ROWS: usize = 128;
const HISTORY_BYTES: usize = 8 * 1024 * 1024;
const REVIEW_BYTES: usize = 16 * 1024 * 1024;

#[derive(Clone, Copy, Default)]
pub(in crate::tenancy) struct Allowance {
    pub rows: usize,
    pub bytes: usize,
}
pub(in crate::tenancy) struct History {
    pub values: Vec<AssessmentRevision>,
    bytes: usize,
}
fn query(a: &Association, session: &str, checkpoint: &str) -> Select<revisions::Entity> {
    revisions::Entity::find()
        .filter(a.predicate("acp_assessment_revisions"))
        .filter(revisions::Column::SessionKey.eq(session))
        .filter(revisions::Column::CheckpointId.eq(checkpoint))
        .order_by_asc(revisions::Column::CreatedAt)
        .order_by_asc(revisions::Column::RevisionId)
}
fn validate(a: &Association, session: &str, row: &revisions::Model) -> Result<AssessmentRevision> {
    a.validate(
        row.installation_id.as_deref(),
        row.tenant_id.as_deref(),
        row.creator_principal_id.as_deref(),
    )?;
    ensure!(
        row.session_key == session && row.revision_json.len() <= REVISION_BYTES,
        "assessment revision parent or bound mismatch"
    );
    let value: AssessmentRevision = serde_json::from_str(&row.revision_json)?;
    let expected = match value.input.source {
        AssessmentSource::Human => a.key(
            KeyDomain::AssessmentRevision,
            &[session, "human-v1", &value.input.submission_id],
        )?,
        AssessmentSource::Agentic => a.key(
            KeyDomain::AssessmentRevision,
            &[session, &value.input.submission_id],
        )?,
    };
    ensure!(
        value.revision_id == expected
            && row.revision_id == expected
            && row.checkpoint_id == value.input.checkpoint_id
            && row.created_at == value.created_at
            && value.input_digest == crate::storage::evolution::digest(&value.input)?
            && io::json(&value, REVISION_BYTES)? == row.revision_json,
        "assessment revision identity or encoding mismatch"
    );
    chrono::DateTime::parse_from_rfc3339(&value.created_at)?;
    Ok(value)
}
pub(in crate::tenancy) async fn revision(
    tx: &DatabaseTransaction,
    a: &Association,
    session: &str,
    id: &str,
    budget: &OperationBudget<'_>,
) -> Result<Option<AssessmentRevision>> {
    let row = io::one(
        tx,
        revisions::Entity::find_by_id(id)
            .filter(a.predicate("acp_assessment_revisions"))
            .filter(revisions::Column::SessionKey.eq(session))
            .lock_exclusive(),
        REVISION_BYTES + 4096,
        budget,
    )
    .await?;
    row.as_ref()
        .map(|row| validate(a, session, row))
        .transpose()
}
pub(in crate::tenancy) async fn load(
    tx: &DatabaseTransaction,
    a: &Association,
    session: &str,
    checkpoint: &str,
    budget: &OperationBudget<'_>,
) -> Result<History> {
    // Numeric lengths only, capped at129 rows BEFORE any history body transfer.
    let selected = query(a, session, checkpoint)
        .select_only()
        .column(revisions::Column::RevisionJson)
        .limit(129)
        .into_query();
    let size = match tx.get_database_backend() {
        DatabaseBackend::Sqlite => "CAST(length(CAST(revision_json AS BLOB)) AS BIGINT)",
        DatabaseBackend::Postgres => "CAST(octet_length(revision_json) AS BIGINT)",
        DatabaseBackend::MySql => "CAST(OCTET_LENGTH(revision_json) AS SIGNED)",
    };
    let sql = Query::select()
        .expr_as(Expr::cust(size), Alias::new("record_bytes"))
        .from_subquery(selected, Alias::new("bounded_history"))
        .to_owned();
    budget.check()?;
    let lengths = tx.query_all(tx.get_database_backend().build(&sql)).await?;
    budget.check()?;
    budget.charge(
        lengths.len(),
        lengths.len().checked_mul(128).ok_or(Error::Capacity)?,
    )?;
    ensure!(lengths.len() <= HISTORY_ROWS, Error::Capacity);
    let mut bytes = 0usize;
    for row in &lengths {
        let length = usize::try_from(row.try_get::<i64>("", "record_bytes")?)?;
        ensure!(
            length <= REVISION_BYTES,
            "assessment revision exceeds retained bound"
        );
        bytes = bytes.checked_add(length).ok_or(Error::Capacity)?;
        ensure!(bytes <= HISTORY_BYTES, Error::Capacity);
    }
    let rows = io::rows(
        tx,
        query(a, session, checkpoint),
        HISTORY_ROWS,
        HISTORY_BYTES + 4096 * HISTORY_ROWS,
        budget,
    )
    .await?;
    ensure!(
        rows.len() == lengths.len(),
        "assessment history snapshot changed"
    );
    let mut actual_bytes = 0usize;
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        actual_bytes = actual_bytes
            .checked_add(row.revision_json.len())
            .ok_or(Error::Capacity)?;
        values.push(validate(a, session, &row)?);
    }
    ensure!(
        actual_bytes == bytes,
        "assessment history byte projection changed"
    );
    Ok(History { values, bytes })
}

#[derive(Serialize)]
struct ScoringProjection<'a> {
    rubric_version: &'a str,
    library: &'a [crate::core::evolution::rubric::Criterion],
    expected_revision: Option<&'a str>,
    evidence: &'a crate::core::evolution::evidence::EvidencePacket,
}
#[derive(Serialize)]
struct Projection<'a> {
    checkpoint: &'a Summary,
    scoring: ScoringProjection<'a>,
    previous: Option<&'a RubricEvaluation>,
    prefill_revision: Option<&'a str>,
    history: &'a [AssessmentRevision],
    current_watermark: i64,
}
fn projection<'a>(
    summary: &'a Summary,
    scoring: &'a ScoringInput,
    history: &'a [AssessmentRevision],
    expected: Option<&'a str>,
    prefill: Option<&'a str>,
    previous: Option<&'a RubricEvaluation>,
    watermark: i64,
) -> Projection<'a> {
    Projection {
        checkpoint: summary,
        scoring: ScoringProjection {
            rubric_version: scoring.rubric_version,
            library: &scoring.library,
            expected_revision: expected,
            evidence: &scoring.evidence,
        },
        previous,
        prefill_revision: prefill,
        history,
        current_watermark: watermark,
    }
}

pub(in crate::tenancy) fn admit(
    summary: &Summary,
    scoring: &ScoringInput,
    history: &History,
    owed: Allowance,
    added: Allowance,
) -> Result<()> {
    let rows = history
        .values
        .len()
        .checked_add(owed.rows)
        .and_then(|n| n.checked_add(added.rows))
        .ok_or(Error::Capacity)?;
    let bytes = history
        .bytes
        .checked_add(owed.bytes)
        .and_then(|n| n.checked_add(added.bytes))
        .ok_or(Error::Capacity)?;
    ensure!(
        rows <= HISTORY_ROWS && bytes <= HISTORY_BYTES,
        Error::Capacity
    );
    // Six encoded bytes per ASCII control character is JSON's maximum escaping
    // for a bounded256-byte ID. These placeholders are counting data only.
    let id = "\0".repeat(256);
    let fixed = io::json_len(
        &projection(summary, scoring, &[], Some(&id), Some(&id), None, i64::MAX),
        REVIEW_BYTES,
    )
    .map_err(|_| Error::Capacity)?;
    // [] and null are already counted, so keeping them safely overcounts four
    // null bytes/two brackets. Exactly one <=1MiB supported prefill is reserved.
    let reply = fixed
        .checked_add(bytes)
        .and_then(|n| n.checked_add(rows.saturating_sub(1)))
        .and_then(|n| n.checked_add(REVISION_BYTES))
        .ok_or(Error::Capacity)?;
    ensure!(reply <= REVIEW_BYTES, Error::Capacity);
    // Future review: <=512 cited event rows, length+body per history row, and
    //32 fixed parent/cursor/member/head/metadata rows. Rows include preflights.
    let read_rows = 512usize
        .checked_add(rows.checked_mul(2).ok_or(Error::Capacity)?)
        .and_then(|n| n.checked_add(32))
        .ok_or(Error::Capacity)?;
    // Actual capture bytes<=8MiB; event cell overhead<=4096 each; history
    // cells<=4096 each; fixed parent/cursor/head-revision/manifest transfers<=2MiB.
    let read_bytes = (8usize * 1024 * 1024 + 512 * 4096 + 2 * 1024 * 1024)
        .checked_add(bytes)
        .and_then(|n| n.checked_add(rows.checked_mul(4096)?))
        .ok_or(Error::Capacity)?;
    ensure!(
        read_rows <= 1024 && read_bytes <= 32 * 1024 * 1024,
        Error::Capacity
    );
    Ok(())
}
pub(in crate::tenancy) fn check_reply(summary: &Summary, value: &CheckpointReview) -> Result<()> {
    if let Some(previous) = &value.previous {
        io::json_len(previous, REVISION_BYTES).map_err(|_| Error::Capacity)?;
    }
    io::json_len(
        &projection(
            summary,
            &value.scoring,
            &value.history,
            value.scoring.expected_revision.as_deref(),
            value.prefill_revision.as_deref(),
            value.previous.as_ref(),
            value.current_watermark,
        ),
        REVIEW_BYTES,
    )
    .map_err(|_| Error::Capacity)?;
    Ok(())
}
pub(in crate::tenancy) fn prefill(
    values: &[AssessmentRevision],
    current: Option<&str>,
) -> Result<(Option<String>, Option<RubricEvaluation>)> {
    let selected = current
        .and_then(|id| values.iter().find(|value| value.revision_id == id))
        .or_else(|| {
            values
                .iter()
                .rev()
                .find(|value| value.input.source == AssessmentSource::Human)
        })
        .or_else(|| values.last());
    let Some(selected) = selected else {
        return Ok((None, None));
    };
    let evaluation = selected
        .input
        .assessment
        .as_ref()
        .map(|assessment| -> Result<RubricEvaluation> {
            ensure!(
                assessment.explanation.len() <= REVISION_BYTES,
                "stored rubric exceeds prefill bound"
            );
            let value: RubricEvaluation = serde_json::from_str(&assessment.explanation)
                .context("retained structured rubric is invalid")?;
            ensure!(
                value.rubric_version == crate::core::evolution::rubric::RUBRIC_VERSION,
                "unsupported retained rubric version"
            );
            Ok(value)
        })
        .transpose()?;
    Ok((Some(selected.revision_id.clone()), evaluation))
}
