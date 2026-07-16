//! Bounded request-scoped reconciliation before benchmark artifact assembly.

use std::collections::BTreeSet;
use std::time::Duration;

use bitrouter_cloud_sdk::settlement::{SettlementClient, SettlementState};
use bitrouter_sdk::{BitrouterError, Result};

use super::{MeteringStore, ReconciliationStatus, UsagePriceOverride};

/// Terminal counts produced by one exact request-id reconciliation batch.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ReconciliationSummary {
    /// Requested rows.
    pub requested: usize,
    /// Rows with exact reconstructed authoritative charges.
    pub computed: usize,
    /// Rows authoritatively confirmed as uncharged.
    pub not_charged: usize,
    /// Rows that exhausted the evidence budget or returned unknown.
    pub unknown: usize,
    /// Total durable HTTP attempts across the selected rows.
    pub attempts: u64,
}

impl ReconciliationSummary {
    /// Whether every selected row reached an accepted terminal state.
    pub fn accepted(self) -> bool {
        self.requested == self.computed + self.not_charged && self.unknown == 0
    }
}

/// Reconcile exactly `request_ids`, bounded by a durable per-row attempt cap.
pub async fn reconcile_requests(
    store: &MeteringStore,
    client: &SettlementClient,
    request_ids: &[String],
    prices: &[UsagePriceOverride],
    max_attempts: u32,
    poll_interval: Duration,
) -> Result<ReconciliationSummary> {
    if request_ids.is_empty() {
        return Err(BitrouterError::bad_request(
            "reconciliation requires at least one request id",
        ));
    }
    if max_attempts == 0 {
        return Err(BitrouterError::bad_request(
            "reconciliation max attempts must be positive",
        ));
    }
    let unique: BTreeSet<_> = request_ids.iter().cloned().collect();
    if unique.len() != request_ids.len() {
        return Err(BitrouterError::bad_request(
            "reconciliation request ids must be unique",
        ));
    }

    loop {
        let records = store.reconciliation_records(request_ids).await?;
        if records.len() != unique.len() {
            let found: BTreeSet<_> = records
                .iter()
                .map(|record| record.request_id.as_str())
                .collect();
            let missing = unique
                .iter()
                .filter(|request_id| !found.contains(request_id.as_str()))
                .cloned()
                .collect::<Vec<_>>()
                .join(",");
            return Err(BitrouterError::bad_request(format!(
                "reconciliation rows missing for request ids: {missing}"
            )));
        }

        let mut progressed = false;
        for record in records {
            match record.status {
                ReconciliationStatus::Pending => {}
                ReconciliationStatus::Computed
                | ReconciliationStatus::NotCharged
                | ReconciliationStatus::Unknown => continue,
                ReconciliationStatus::NotApplicable => {
                    return Err(BitrouterError::bad_request(format!(
                        "request {} does not require authoritative reconciliation",
                        record.request_id
                    )));
                }
            }
            if record.attempts >= max_attempts {
                store
                    .exhaust_reconciliation(&record.request_id, "attempt_budget_exhausted")
                    .await?;
                progressed = true;
                continue;
            }
            let attempt = store
                .start_reconciliation_attempt(&record.request_id)
                .await?;
            progressed = true;
            match client.get(&record.request_id).await {
                Ok(receipt) if receipt.state == SettlementState::Pending => {
                    if attempt >= max_attempts {
                        store
                            .exhaust_reconciliation(
                                &record.request_id,
                                "authoritative_receipt_still_pending",
                            )
                            .await?;
                    }
                }
                Ok(receipt) => {
                    store.apply_authoritative_receipt(&receipt, prices).await?;
                }
                Err(error) => {
                    store
                        .record_reconciliation_error(&record.request_id, &error.to_string())
                        .await?;
                    if attempt >= max_attempts {
                        store
                            .exhaust_reconciliation(
                                &record.request_id,
                                "receipt_fetch_attempts_exhausted",
                            )
                            .await?;
                    }
                }
            }
        }

        let records = store.reconciliation_records(request_ids).await?;
        if records
            .iter()
            .all(|record| record.status != ReconciliationStatus::Pending)
        {
            let mut summary = ReconciliationSummary {
                requested: records.len(),
                attempts: records
                    .iter()
                    .map(|record| u64::from(record.attempts))
                    .sum(),
                ..Default::default()
            };
            for record in records {
                match record.status {
                    ReconciliationStatus::Computed => summary.computed += 1,
                    ReconciliationStatus::NotCharged => summary.not_charged += 1,
                    ReconciliationStatus::Unknown => summary.unknown += 1,
                    ReconciliationStatus::Pending | ReconciliationStatus::NotApplicable => {}
                }
            }
            return Ok(summary);
        }
        if !progressed {
            return Err(BitrouterError::internal(
                "reconciliation made no progress while rows remained pending",
            ));
        }
        tokio::time::sleep(poll_interval).await;
    }
}
