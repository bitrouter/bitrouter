//! Read-only projection of existing, caller-owned settlement evidence.

use super::entities::requests;
use super::pricing::{ChargeEvidence, ChargeStatus, PricingSource};
use super::store::MeteringStore;
use crate::cloud::settlement::{SettlementReceipt, SettlementState, validate_receipt};
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::native_accounting::{
    NativeCostBasis, NativeCostClaim, NativeCostObservation, NativeCostScope, NativeCostSource,
};

#[async_trait::async_trait]
impl NativeCostSource for MeteringStore {
    async fn read(
        &self,
        caller: &CallerContext,
        ids: &[String],
    ) -> bitrouter_sdk::Result<Vec<NativeCostObservation>> {
        let rows = self.native_request_rows(caller, ids).await?;
        ids.iter()
            .map(|id| {
                let Some(row) = rows.iter().find(|row| &row.request_id == id) else {
                    return Ok(NativeCostObservation {
                        request_id: id.clone(),
                        claims: Vec::new(),
                        unknown_reason: Some("settlement_unavailable".into()),
                    });
                };
                observation(row)
            })
            .collect()
    }
}

fn claim(
    row: &requests::Model,
    basis: NativeCostBasis,
    scope: NativeCostScope,
    amount: u64,
    evidence: &impl serde::Serialize,
) -> bitrouter_sdk::Result<NativeCostClaim> {
    Ok(NativeCostClaim {
        request_id: row.request_id.clone(),
        source: "request_metering_v1".into(),
        bill_id: row.request_id.clone(),
        basis,
        scope,
        micro_usd: amount,
        evidence_sha256: crate::eval::types::canonical_digest(evidence)
            .map_err(|_| bitrouter_sdk::BitrouterError::internal("invalid monetary evidence"))?,
        provider: row.provider_id.clone(),
        model: row.model_id.clone(),
    })
}

fn observation(row: &requests::Model) -> bitrouter_sdk::Result<NativeCostObservation> {
    let mut result = NativeCostObservation {
        request_id: row.request_id.clone(),
        claims: Vec::new(),
        unknown_reason: Some("charge_not_reconciled".into()),
    };
    let evidence = row
        .charge_evidence_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<ChargeEvidence>(json).ok());
    if let Some(evidence) = &evidence
        && row.charge_status == "computed"
        && evidence.status == ChargeStatus::Computed
        && matches!(
            evidence.pricing_source,
            PricingSource::Configured | PricingSource::Override
        )
        && matches!(row.usage_origin.as_str(), "provider_reported" | "estimated")
        && !synthetic_rejection(row)
        && let Some(amount) = evidence
            .charge_micro_usd
            .and_then(|amount| u64::try_from(amount).ok())
    {
        result.claims.push(claim(
            row,
            NativeCostBasis::Estimated,
            NativeCostScope::ModelTokens,
            amount,
            evidence,
        )?);
    }
    let receipt = row
        .authoritative_receipt_json
        .as_deref()
        .and_then(|json| serde_json::from_str::<SettlementReceipt>(json).ok());
    let Some(receipt) =
        receipt.filter(|receipt| validate_receipt(&row.request_id, receipt).is_ok())
    else {
        return Ok(result);
    };
    let amount = match receipt.state {
        SettlementState::Computed => receipt
            .final_charge_micro_usd
            .and_then(|amount| u64::try_from(amount).ok()),
        SettlementState::NotCharged
            if [
                receipt.usage.uncached_input_tokens,
                receipt.usage.cache_read_tokens,
                receipt.usage.cache_write_tokens,
                receipt.usage.output_tokens,
                receipt.usage.reasoning_tokens,
            ]
            .into_iter()
            .all(|tokens| tokens == 0) =>
        {
            Some(0)
        }
        _ => None,
    };
    let Some(amount) = amount else {
        return Ok(result);
    };
    // The stored receipt is a report even when local reconciliation rejected it.
    // Its request identity is validated by the existing receipt contract.
    let mut reported = claim(
        row,
        NativeCostBasis::Reported,
        NativeCostScope::RequestBill,
        amount,
        &receipt,
    )?;
    reported.provider = receipt.provider_id.clone().unwrap_or_default();
    reported.model = receipt.model_id.clone().unwrap_or_default();
    result.claims.push(reported.clone());
    let reconciled = row.usage_origin == "authoritative_receipt"
        && row.authoritative_settled_at.is_some()
        && match receipt.state {
            SettlementState::Computed => {
                row.reconciliation_status == "computed"
                    && row.charge_status == "computed"
                    && evidence.as_ref().is_some_and(|evidence| {
                        evidence.status == ChargeStatus::Computed
                            && evidence
                                .charge_micro_usd
                                .and_then(|charge| u64::try_from(charge).ok())
                                == Some(amount)
                    })
                    && receipt.provider_id.as_deref() == Some(row.provider_id.as_str())
                    && receipt.model_id.as_deref() == Some(row.model_id.as_str())
            }
            SettlementState::NotCharged => {
                row.reconciliation_status == "not_charged"
                    && row.charge_status == "not_charged"
                    && evidence
                        .as_ref()
                        .is_some_and(|evidence| evidence.status == ChargeStatus::NotCharged)
            }
            _ => false,
        };
    if reconciled {
        reported.basis = NativeCostBasis::Reconciled;
        result.claims.push(reported);
        result.unknown_reason = None;
    }
    Ok(result)
}

fn synthetic_rejection(row: &requests::Model) -> bool {
    // The legacy recorder normalizes usage-free rejections for compatibility.
    // That synthesized zero is not an observed token count or a free bill.
    row.raw_usage_json
        .as_deref()
        .and_then(|raw| serde_json::from_str::<serde_json::Value>(raw).ok())
        .is_some_and(|raw| {
            raw.get("usage").is_some_and(serde_json::Value::is_null)
                && matches!(
                    raw.pointer("/error/code")
                        .and_then(serde_json::Value::as_str),
                    Some("upstream_policy_violation" | "upstream_rate_limited")
                )
        })
}
