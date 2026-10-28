//! Negative-only edits after the genuine predecessor has been killed and reaped.
use super::*;
use sea_orm::{DatabaseTransaction, Value as SqlValue};
use serde::{Deserialize, Serialize};

#[derive(Clone, Copy)]
pub(super) enum Change {
    Tenant,
    Principal,
    Evidence,
    Receipt,
    Mirror,
    CloseAck,
    Epoch,
    StopSet,
    ExtraMember,
}
impl Change {
    pub(super) fn parse(name: &str) -> Result<Self> {
        Ok(match name {
            "tenant" => Self::Tenant,
            "principal" => Self::Principal,
            "evidence" => Self::Evidence,
            "receipt" => Self::Receipt,
            "mirror" => Self::Mirror,
            "close-ack" => Self::CloseAck,
            "epoch" => Self::Epoch,
            "stop-set" => Self::StopSet,
            "extra-member" => Self::ExtraMember,
            _ => anyhow::bail!("unknown native integrity case"),
        })
    }
}
struct Row {
    key: String,
    body: String,
    revision: i64,
}
async fn row(tx: &DatabaseTransaction, kind: &str) -> Result<Row> {
    let rows = tx.query_all(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT record_id,revision,length(CAST(body AS BLOB)) AS bytes FROM checkpoint_evolution WHERE kind=? LIMIT 2",
        [kind.into()])).await?;
    ensure!(rows.len() == 1);
    let bytes = rows[0].try_get::<i64>("", "bytes")?;
    ensure!((1..=1024 * 1024).contains(&bytes));
    let key: String = rows[0].try_get("", "record_id")?;
    let revision: i64 = rows[0].try_get("", "revision")?;
    let body = tx.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT body FROM checkpoint_evolution WHERE record_id=? AND kind=? AND revision=? AND length(CAST(body AS BLOB))=?",
        [key.as_str().into(),kind.into(),SqlValue::from(revision),bytes.into()])).await?
        .context("bounded original record")?.try_get::<String>("","body")?;
    ensure!(body.len() == usize::try_from(bytes)?);
    Ok(Row {
        key,
        body,
        revision,
    })
}
async fn json(
    tx: &DatabaseTransaction,
    body: &str,
    path: &str,
    replacement: Option<&str>,
) -> Result<String> {
    // Check SQLite's roundtrip on the ACTUAL original scalar/object before
    // modifying it. A reordering/encoding failure must fail setup, not be
    // mistaken for the recovery guard under test.
    let same = tx
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT json_set(?, ?, json(json_quote(json_extract(?, ?)))) AS body",
            [body.into(), path.into(), body.into(), path.into()],
        ))
        .await?
        .context("JSON roundtrip")?
        .try_get::<String>("", "body")?;
    ensure!(
        same == body,
        "negative JSON setup changed canonical source encoding"
    );
    let result = match replacement {
        Some(value) => {
            tx.query_one(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT json_set(?, ?, json(?)) AS body",
                [body.into(), path.into(), value.into()],
            ))
            .await?
        }
        None => {
            tx.query_one(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                "SELECT json_remove(?, ?) AS body",
                [body.into(), path.into()],
            ))
            .await?
        }
    }
    .context("negative JSON projection")?
    .try_get::<String>("", "body")?;
    ensure!(result != body && result.len() <= 1024 * 1024);
    Ok(result)
}
async fn replace(tx: &DatabaseTransaction, row: &Row, body: &str, revision: i64) -> Result<()> {
    let changed = tx.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE checkpoint_evolution SET body=?,revision=? WHERE record_id=? AND body=? AND revision=?",
        [SqlValue::from(body),revision.into(),row.key.as_str().into(),row.body.as_str().into(),row.revision.into()])).await?;
    ensure!(
        changed.rows_affected() == 1,
        "negative update did not target the exact original row"
    );
    Ok(())
}
// Test-only data copies, never an owner/proof constructor. Explicit field order
// is verified against the original stored object's bytes before any forgery.
#[derive(Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Member {
    tenant: String,
    execution: String,
    job: String,
    seed_digest: String,
}
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
struct Stopped {
    members: Vec<Member>,
    set_digest: String,
    local_cleanup_digest: String,
}
async fn extra_member(tx: &DatabaseTransaction, value: &Value, body: &str) -> Result<String> {
    let raw = tx
        .query_one(Statement::from_sql_and_values(
            DatabaseBackend::Sqlite,
            "SELECT json_extract(?, '$.recovery.stopped') AS body",
            [body.into()],
        ))
        .await?
        .context("actual stop set")?
        .try_get::<String>("", "body")?;
    let mut stopped: Stopped = serde_json::from_str(&raw)?;
    ensure!(serde_json::to_string(&stopped)? == raw && stopped.members.len() == 1);
    let origin: crate::tenancy::origin::Stamp =
        serde_json::from_value(value["recovery"]["seed"]["origin"].clone())?;
    ensure!(crate::storage::evolution::digest(&(&origin, &stopped.members))? == stopped.set_digest);
    let mut extra = stopped.members[0].clone();
    extra.execution.push_str("-extra");
    extra.job.push_str("-extra");
    ensure!(extra.execution.len() <= 128 && extra.job.len() <= 256);
    let _: bitrouter_ai::tenancy::identity::ExecutionId = extra.execution.as_str().try_into()?;
    ensure!(tx.query_one(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "SELECT 1 FROM checkpoint_evolution WHERE record_id=? OR json_extract(body,'$.execution')=? LIMIT 1",
        [extra.job.as_str().into(),extra.execution.as_str().into()])).await?.is_none());
    stopped.members.push(extra);
    stopped
        .members
        .sort_by(|a, b| a.execution.cmp(&b.execution));
    stopped.set_digest = crate::storage::evolution::digest(&(&origin, &stopped.members))?;
    ensure!(
        stopped.members.len() == 2 && stopped.members[0].execution != stopped.members[1].execution
    );
    Ok(serde_json::to_string(&stopped)?)
}

pub(super) async fn apply(
    tx: &DatabaseTransaction,
    change: Change,
    accepted: &Value,
) -> Result<()> {
    let active = row(tx, "owned_judge_active_v3").await?;
    let pending = row(tx, "owned_judge_pending").await?;
    let original: Value = serde_json::from_str(&active.body)?;
    ensure!(
        active.body == pending.body
            && active.revision == pending.revision
            && active.revision == 3
            && original == *accepted
            && original["version"] == 4
            && original["status"] == "accepted"
            && original["recovery"]["native_cleanup"].is_object()
            && original["recovery"]["stopped"].is_object()
            && original["recovery"]["observed"].is_object()
            && original["native_evidence"].is_object()
            && original["output"]["usage"]["prompt_tokens"] == 4
    );
    let seed = &original["recovery"]["seed"];
    let installation = seed["installation"]
        .as_str()
        .context("actual installation")?
        .try_into()?;
    let tenant = seed["tenant"]
        .as_str()
        .context("actual tenant")?
        .try_into()?;
    let execution = seed["execution"]
        .as_str()
        .context("actual paid execution")?
        .try_into()?;
    let receipt = crate::tenancy::receipts::store::lookup(tx, &installation, &tenant, &execution)
        .await?
        .context("actual original035 receipt")?;
    ensure!(
        receipt.state == bitrouter_ai::tenancy::execution::ReceiptState::Committed
            && receipt.usage.input_tokens == Some(4)
            && receipt.usage.output_tokens == Some(2)
    );
    if matches!(change, Change::Tenant | Change::Principal) {
        let (column, value) = if matches!(change, Change::Tenant) {
            ("tenant_id", "corrupt-tenant")
        } else {
            ("creator_principal_id", "corrupt-principal")
        };
        ensure!(
            seed[if matches!(change, Change::Tenant) {
                "tenant"
            } else {
                "principal"
            }] != value
        );
        let result = tx
            .execute(Statement::from_sql_and_values(
                DatabaseBackend::Sqlite,
                format!("UPDATE checkpoint_evolution SET {column}=? WHERE record_id=? AND body=?"),
                [
                    value.into(),
                    active.key.as_str().into(),
                    active.body.as_str().into(),
                ],
            ))
            .await?;
        ensure!(result.rows_affected() == 1);
        return Ok(());
    }
    let (path, replacement, revision) = match change {
        Change::Evidence => (
            "$.native_evidence.items[0].method",
            Some(serde_json::to_string("native.integrity-corruption")?),
            active.revision,
        ),
        Change::Receipt => {
            let previous = original["recovery"]["observed"]["receipt_digest"]
                .as_str()
                .context("observed digest")?;
            ensure!(previous.len() == 64);
            let mut digest = previous.as_bytes().to_vec();
            digest[0] = if digest[0] == b'0' { b'1' } else { b'0' };
            (
                "$.recovery.observed.receipt_digest",
                Some(serde_json::to_string(&String::from_utf8(digest)?)?),
                active.revision,
            )
        }
        Change::Mirror => (
            "$.accepted_at",
            Some(serde_json::to_string("2000-01-01T00:00:00Z")?),
            active.revision,
        ),
        Change::CloseAck => (
            "$.recovery.native_cleanup",
            None,
            active.revision.checked_sub(1).context("record revision")?,
        ),
        Change::Epoch => {
            let epoch = original["recovery"]["native_cleanup"]["grant"]["execution_epoch"]
                .as_u64()
                .context("actual cleanup epoch")?;
            (
                "$.recovery.native_cleanup.grant.execution_epoch",
                Some(epoch.checked_add(1).context("epoch bound")?.to_string()),
                active.revision,
            )
        }
        Change::StopSet => (
            "$.recovery.stopped",
            None,
            active.revision.checked_sub(1).context("record revision")?,
        ),
        Change::ExtraMember => (
            "$.recovery.stopped",
            Some(extra_member(tx, &original, &active.body).await?),
            active.revision,
        ),
        Change::Tenant | Change::Principal => anyhow::bail!("projection case already handled"),
    };
    let body = json(tx, &active.body, path, replacement.as_deref()).await?;
    if !matches!(change, Change::Mirror) {
        replace(tx, &active, &body, revision).await?;
    }
    replace(tx, &pending, &body, revision).await?;
    let after = row(tx, "owned_judge_active_v3").await?;
    let mirror = row(tx, "owned_judge_pending").await?;
    ensure!(if matches!(change, Change::Mirror) {
        after.body == active.body && mirror.body != after.body
    } else {
        after.body == body
            && mirror.body == body
            && after.revision == revision
            && mirror.revision == revision
    });
    // No fixture branch touches the genuine receipt, source or cumulative budget.
    let preserved = crate::tenancy::receipts::store::lookup(tx, &installation, &tenant, &execution)
        .await?
        .context("preserved actual035")?;
    ensure!(serde_json::to_value(preserved)? == serde_json::to_value(receipt)?);
    Ok(())
}
