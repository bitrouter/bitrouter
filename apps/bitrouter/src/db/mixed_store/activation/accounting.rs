//! A settlement is retained evidence. It never authorizes a new provider send.
//! Local token estimates and final request bills are independent monetary bases.
use crate::cloud::settlement::{SettlementReceipt, SettlementState, validate_receipt};
use crate::gateway::observability::inventory::{GatewayInventory, GatewayRequest, INVENTORY_KIND};
use crate::metering::entities::requests;
use crate::metering::pricing::{ChargeEvidence, ChargeStatus, PricingSource};
use crate::storage::evolution::records;
use anyhow::{Context, Result, bail, ensure};
use futures::TryStreamExt;
use sea_orm::{ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, QueryOrder, QuerySelect};

pub(super) async fn verify(db: &DatabaseConnection) -> Result<()> {
    let mut rows = requests::Entity::find().stream(db).await?;
    while let Some(row) = rows.try_next().await? {
        meter(&row).with_context(|| {
            format!(
                "unsettled or inconsistent model charge for {}",
                row.request_id
            )
        })?;
    }
    drop(rows);
    // Release the cursor before the associated metering lookup on single-connection SQLite.
    let mut after = None;
    loop {
        let mut query = records::Entity::find().filter(records::Column::Kind.eq(INVENTORY_KIND));
        if let Some(key) = after.as_ref() {
            query = query.filter(records::Column::RecordId.gt(key));
        }
        let batch = query
            .order_by_asc(records::Column::RecordId)
            .limit(64)
            .all(db)
            .await?;
        if batch.is_empty() {
            break;
        }
        for row in batch {
            after = Some(row.record_id.clone());
            inventory(db, &row)
                .await
                .context("unsupported gateway request history")?;
        }
    }
    Ok(())
}

fn stamp(value: &str) -> Result<chrono::DateTime<chrono::FixedOffset>> {
    Ok(chrono::DateTime::parse_from_rfc3339(value)?)
}

fn meter(row: &requests::Model) -> Result<()> {
    ensure!(
        !row.request_id.is_empty() && !row.user_id.is_empty() && !row.api_key_id.is_empty(),
        "missing metering identity"
    );
    let evidence: ChargeEvidence = serde_json::from_str(
        row.charge_evidence_json
            .as_deref()
            .context("missing charge evidence")?,
    )?;
    let usage = &evidence.normalized_usage;
    let inputs = [
        row.uncached_input_tokens,
        row.cache_read_tokens,
        row.cache_write_tokens,
    ];
    ensure!(
        inputs.iter().all(|n| *n >= 0)
            && row.output_tokens >= 0
            && row.reasoning_tokens >= 0
            && inputs.iter().try_fold(0_i64, |n, x| n.checked_add(*x)) == Some(row.prompt_tokens)
            && row.output_tokens.checked_add(row.reasoning_tokens) == Some(row.completion_tokens)
            && [
                usage.uncached_input_tokens,
                usage.cache_read_tokens,
                usage.cache_write_tokens,
                usage.output_tokens,
                usage.reasoning_tokens
            ] == [
                inputs[0] as u64,
                inputs[1] as u64,
                inputs[2] as u64,
                row.output_tokens as u64,
                row.reasoning_tokens as u64
            ],
        "token ledger mismatch"
    );
    ensure!(
        row.charge_status == evidence.status.as_str()
            && match evidence.status {
                ChargeStatus::LegacyUnknown => false,
                ChargeStatus::Computed =>
                    evidence
                        .charge_micro_usd
                        .is_some_and(|n| n >= 0 && n == row.estimated_charge_micro_usd)
                        && evidence.pricing_source != PricingSource::Unknown
                        && evidence.unknown_reason.is_none(),
                ChargeStatus::Unknown =>
                    evidence.charge_micro_usd.is_none()
                        && row.estimated_charge_micro_usd == 0
                        && evidence
                            .unknown_reason
                            .as_ref()
                            .is_some_and(|r| !r.is_empty()),
                ChargeStatus::NotCharged =>
                    evidence.charge_micro_usd.is_none_or(|n| n == 0)
                        && row.estimated_charge_micro_usd == 0,
            },
        "charge status or amount mismatch"
    );
    if row.reconciliation_status == "not_applicable" {
        ensure!(
            row.authoritative_settled_at.is_none()
                && evidence.pricing_source != PricingSource::AuthoritativeReceipt
                && row.authoritative_receipt_json.is_none()
                && matches!(
                    row.usage_origin.as_str(),
                    "provider_reported" | "estimated" | "unknown"
                ),
            "unexpected authoritative receipt"
        );
        return Ok(());
    }
    stamp(
        row.authoritative_settled_at
            .as_deref()
            .context("no terminal settlement")?,
    )?;
    let Some(raw) = &row.authoritative_receipt_json else {
        ensure!(
            row.reconciliation_status == "unknown"
                && evidence.status == ChargeStatus::Unknown
                && row
                    .reconciliation_last_error
                    .as_ref()
                    .is_some_and(|s| !s.is_empty())
                && matches!(
                    row.usage_origin.as_str(),
                    "provider_reported" | "estimated" | "unknown"
                ),
            "terminal receipt is missing"
        );
        return Ok(());
    };
    let receipt: SettlementReceipt = serde_json::from_str(raw)?;
    validate_receipt(&row.request_id, &receipt)?;
    let source: serde_json::Value = serde_json::from_str(raw)?;
    ensure!(
        serde_json::to_value(&receipt)? == source,
        "unrecognized authoritative settlement evidence"
    );
    ensure!(
        row.usage_origin == "authoritative_receipt"
            && serde_json::from_str::<serde_json::Value>(
                row.raw_usage_json.as_deref().context("no receipt usage")?
            )? == source
            && receipt.model_id.as_ref().is_none_or(|m| m == &row.model_id)
            && receipt
                .provider_id
                .as_ref()
                .is_none_or(|p| p == &row.provider_id)
            && [
                receipt.usage.uncached_input_tokens,
                receipt.usage.cache_read_tokens,
                receipt.usage.cache_write_tokens,
                receipt.usage.output_tokens,
                receipt.usage.reasoning_tokens
            ] == [
                inputs[0],
                inputs[1],
                inputs[2],
                row.output_tokens,
                row.reasoning_tokens
            ],
        "authoritative receipt conflicts with metering"
    );
    if evidence.pricing_source == PricingSource::AuthoritativeReceipt {
        ensure!(
            receipt.state == SettlementState::Computed
                && evidence.pricing_version
                    == crate::storage::evaluation::canonical_digest(&receipt)?,
            "authoritative charge evidence commitment mismatch"
        );
    }
    match receipt.state {
        SettlementState::Computed => ensure!(
            row.reconciliation_status == "computed"
                && evidence.status == ChargeStatus::Computed
                && receipt.final_charge_micro_usd == evidence.charge_micro_usd
                && receipt.provider_id.is_some()
                && receipt.model_id.is_some()
                && row.reconciliation_last_error.is_none(),
            "conflicting computed bill"
        ),
        SettlementState::NotCharged => ensure!(
            row.reconciliation_status == "not_charged"
                && evidence.status == ChargeStatus::NotCharged
                && row.prompt_tokens == 0
                && row.completion_tokens == 0
                && row.reconciliation_last_error.is_none(),
            "conflicting no-charge bill"
        ),
        SettlementState::Unknown => ensure!(
            row.reconciliation_status == "unknown"
                && evidence.status == ChargeStatus::Unknown
                && row.reconciliation_last_error.as_deref()
                    == Some("authoritative_settlement_unknown")
                && evidence.unknown_reason == row.reconciliation_last_error,
            "conflicting Unknown receipt"
        ),
        SettlementState::Pending => bail!("receipt is in transit"),
    }
    Ok(())
}

async fn inventory(db: &DatabaseConnection, row: &records::Model) -> Result<()> {
    let request: GatewayRequest = serde_json::from_str(&row.body)?;
    ensure!(
        serde_json::to_value(&request)? == serde_json::from_str::<serde_json::Value>(&row.body)?,
        "unknown gateway inventory fields"
    );
    let owner = GatewayInventory::store_for(
        db.clone(),
        &request.route_scope_id,
        &request.controller_instance_id,
    )?;
    ensure!(
        row.scope_id == owner.scope_id
            && row.record_id == owner.id(INVENTORY_KIND, &request.request_id)?
            && row.revision == 2
            && request.coverage_ready
            && row.session_key.is_none()
            && request.binding.is_none()
            && request.anchors.is_empty()
            && !request.controller_instance_id.is_empty()
            && !request.route_scope_id.is_empty()
            && !request.request_id.is_empty(),
        "invalid or capture-bound M0 request identity"
    );
    uuid::Uuid::parse_str(&request.attempt_id)?;
    uuid::Uuid::parse_str(&request.runtime_epoch)?;
    let settlement = request.settlement.context("request still in transit")?;
    let bill = requests::Entity::find_by_id(&request.request_id)
        .one(db)
        .await?
        .context("gateway reservation has no metering row")?;
    ensure!(
        bill.route_scope_id.as_deref() == Some(&request.route_scope_id)
            && bill.controller_instance_id.as_deref() == Some(&request.controller_instance_id)
            && settlement.final_provider == bill.provider_id
            && settlement.final_model == bill.model_id
            && stamp(&settlement.settled_at)? >= stamp(&request.admitted_at)?,
        "settlement owner, route or time mismatch"
    );
    let [hop] = settlement.hops.as_slice() else {
        bail!("unsupported multi-hop or unobserved dispatch")
    };
    ensure!(
        matches!(
            hop.protocol.as_str(),
            "chat_completions" | "responses" | "messages" | "decisions"
        ) && hop.model == bill.model_id
            && hop.provider == bill.provider_id
            && hop.error_code == settlement.error_code,
        "conflicting settled hop"
    );
    use crate::storage::execution::ExecutionOutcome;
    match settlement.outcome.context("no terminal gateway outcome")? {
        ExecutionOutcome::Completed => ensure!(
            hop.status == "completed" && settlement.error_code.is_none() && bill.error.is_none(),
            "conflicting completed settlement"
        ),
        ExecutionOutcome::Failed => ensure!(
            hop.status == "failed" && settlement.error_code.is_some() && bill.error.is_some(),
            "conflicting failed settlement"
        ),
        ExecutionOutcome::ClientDisconnected => ensure!(
            hop.status == "incomplete" && settlement.total_cost_micro_usd.is_none(),
            "conflicting disconnect settlement"
        ),
    }
    if let Some(amount) = settlement.total_cost_micro_usd {
        ensure!(
            Some(amount) == settlement.final_metered_cost_micro_usd,
            "inconsistent single-hop total"
        );
    }
    if let Some(amount) = settlement.final_metered_cost_micro_usd {
        ensure!(
            bill.charge_status == "computed"
                && i64::try_from(amount)? == bill.estimated_charge_micro_usd,
            "unbacked historical gateway charge"
        );
    }
    Ok(())
}
