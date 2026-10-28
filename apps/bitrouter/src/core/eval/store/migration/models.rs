//! Exact original cell-to-model projection. JSON and timestamp spelling survive.
use super::*;
fn text(row: &Row, index: usize) -> Result<String> {
    Ok(row.text(index)?.to_owned())
}
pub(super) fn subject(row: &Row) -> Result<subject_entity::Model> {
    let holdout = row.integer(8)?;
    ensure!(matches!(holdout, 0 | 1), "holdout storage value");
    Ok(subject_entity::Model {
        installation_id: row.nullable(0)?,
        tenant_id: row.nullable(1)?,
        creator_principal_id: row.nullable(2)?,
        eval_id: text(row, 3)?,
        owner_user_id: text(row, 4)?,
        subject_id: text(row, 5)?,
        scope: text(row, 6)?,
        policy_digest: text(row, 7)?,
        holdout: holdout == 1,
        content_digest: text(row, 9)?,
        subject_json: text(row, 10)?,
        created_at: text(row, 11)?,
    })
}
pub(super) fn result(row: &Row) -> Result<result_entity::Model> {
    Ok(result_entity::Model {
        installation_id: row.nullable(0)?,
        tenant_id: row.nullable(1)?,
        creator_principal_id: row.nullable(2)?,
        result_id: text(row, 3)?,
        owner_user_id: text(row, 4)?,
        eval_id: text(row, 5)?,
        idempotency_key: text(row, 6)?,
        authority_id: text(row, 7)?,
        evaluator_kind: text(row, 8)?,
        content_digest: text(row, 9)?,
        result_json: text(row, 10)?,
        created_at: text(row, 11)?,
    })
}
pub(super) fn admission(row: &Row) -> Result<admission_entity::Model> {
    Ok(admission_entity::Model {
        installation_id: row.nullable(0)?,
        tenant_id: row.nullable(1)?,
        creator_principal_id: row.nullable(2)?,
        sequence: row.integer(3)?,
        result_id: text(row, 4)?,
        status: text(row, 5)?,
        reason: text(row, 6)?,
        authority_id: text(row, 7)?,
        created_at: text(row, 8)?,
    })
}
pub(super) fn snapshot(row: &Row) -> Result<snapshot_entity::Model> {
    Ok(snapshot_entity::Model {
        installation_id: row.nullable(0)?,
        tenant_id: row.nullable(1)?,
        creator_principal_id: row.nullable(2)?,
        evidence_root: text(row, 3)?,
        owner_user_id: text(row, 4)?,
        manifest_json: text(row, 5)?,
        result_count: i32::try_from(row.integer(6)?)?,
        frozen_at: text(row, 7)?,
    })
}
