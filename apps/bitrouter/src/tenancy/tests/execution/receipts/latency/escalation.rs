//! Real provider-created durable state, real actor inventory, explicit age only.
use super::*;
use bitrouter_ai::tenancy::execution::ReceiptState;
use sea_orm::Statement;

pub(super) async fn restored_unknown_keeps_evidence(
    h: &Harness,
    restarted: &crate::tenancy::Service,
    receipt: &ExecutionReceipt,
    bytes: u64,
) -> Result<()> {
    let unknown = receipt.state == ReceiptState::Unknown;
    let raw = raw_receipt(h, &receipt.execution_id).await?;
    let original = serde_json::to_value(receipt)?;
    let loaded = store::lookup(
        &h.fixture.db,
        &h.fixture.installation,
        &h.fixture.tenant,
        &receipt.execution_id,
    )
    .await?
    .context("actual receipt after actor restart")?;
    ensure!(serde_json::to_value(loaded)? == original);
    let before = restarted
        .admission
        .observed_capacity()
        .map_err(|_| anyhow::anyhow!("actual restored capacity"))?;
    ensure!(before.accounted == bytes && before.live_unknown == u64::from(unknown));
    ensure!(before.active == 0 && before.potential_unknown == 0 && before.completion_reserved == 0);
    ensure!(restarted.admission.next_receipt_escalation().is_some() == unknown);
    // No row timestamp is rewritten. The explicit age input reaches the same
    // Accounting::escalate used by actor maintenance under the same real mutex.
    let due = chrono::DateTime::parse_from_rfc3339(receipt.recorded_at.as_str())?
        .with_timezone(&chrono::Utc)
        + chrono::Duration::minutes(15);
    ensure!(
        restarted
            .admission
            .escalate_receipts_at_for_test(due - chrono::Duration::nanoseconds(1))
            == 0
    );
    ensure!(restarted.admission.observed_capacity() == Ok(before));
    ensure!(restarted.admission.escalate_receipts_at_for_test(due) == usize::from(unknown));
    ensure!(restarted.admission.next_receipt_escalation().is_none());
    ensure!(
        restarted
            .admission
            .escalate_receipts_at_for_test(due + chrono::Duration::minutes(1))
            == 0,
        "already escalated Unknown must not escalate again"
    );
    ensure!(
        restarted.admission.observed_capacity() == Ok(before),
        "escalation must retain Unknown and accounting"
    );
    ensure!(
        raw_receipt(h, &receipt.execution_id).await? == raw,
        "durable evidence changed during escalation"
    );
    let after = store::lookup(
        &h.fixture.db,
        &h.fixture.installation,
        &h.fixture.tenant,
        &receipt.execution_id,
    )
    .await?
    .context("retained receipt after escalation")?;
    ensure!(serde_json::to_value(after)? == original);
    ensure!(
        h.calls.load(Ordering::SeqCst) == 1,
        "escalation must not redispatch"
    );
    ensure!(capacity::stored_policy_bytes(h).await? == bytes);
    Ok(())
}

async fn raw_receipt(h: &Harness, id: &ExecutionId) -> Result<Value> {
    let rows = h.fixture.db.query_all(Statement::from_sql_and_values(
        h.fixture.db.get_database_backend(),
        "SELECT * FROM tenancy_execution_receipts WHERE installation_id=? AND tenant_id=? AND execution_id=?",
        [h.fixture.installation.as_str().into(), h.fixture.tenant.as_str().into(), id.as_str().into()],
    )).await?;
    ensure!(rows.len() == 1, "exact real durable row required");
    let row = &rows[0];
    let mut value = serde_json::Map::new();
    for column in [
        "installation_id",
        "tenant_id",
        "execution_id",
        "principal_id",
        "correlation_id",
        "receipt_json",
        "content_digest",
        "disposition_json",
        "recorded_at",
    ] {
        value.insert(column.into(), Value::String(row.try_get("", column)?));
    }
    value.insert(
        "byte_count".into(),
        json!(row.try_get::<i64>("", "byte_count")?),
    );
    value.insert(
        "unknown_outcome".into(),
        json!(row.try_get::<bool>("", "unknown_outcome")?),
    );
    Ok(Value::Object(value))
}
