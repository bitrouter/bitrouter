//! Reserve all owned terminal commitments within the native journal sub-budgets.
use super::{CoreError, ErrorCode, MAX_LINK_BYTES, MAX_LINK_TOTAL, MAX_RECEIPT, Record};
use crate::storage::checkpoint::{serialized_bytes, snapshot::context::DecisionReceipt};
use std::collections::BTreeMap;

// Matches Core's existing conservative terminal metadata allowance. The bounded
// Result consumes response_limit_bytes; 4096 additionally covers its wrapper,
// fixed sanitized errors (including stopped-owner Interrupted), max-u64 elapsed,
// and stale spelling. No provider body or diagnostic is copied into this reserve.
// It is accounting only: no limit is raised, response altered, or receipt pruned.
const TERMINAL_METADATA: usize = 4096;
// A pending record replaces null with a quoted 64-hex digest. Counting all 66
// bytes without reclaiming null is conservative, for each link and the whole map.
const OUTCOME_DIGEST: usize = 66;

pub(super) fn exhausted() -> CoreError {
    CoreError::rejected(
        ErrorCode::LimitExceeded,
        "owned Choice completion capacity exhausted",
    )
}

pub(super) fn receipt(receipt: &DecisionReceipt, actual: usize) -> Result<usize, CoreError> {
    let projected = if receipt.outcome.is_none() {
        actual
            .checked_add(receipt.response_limit_bytes)
            .and_then(|bytes| bytes.checked_add(TERMINAL_METADATA))
            .ok_or_else(exhausted)?
    } else {
        actual
    };
    if projected > MAX_RECEIPT {
        return Err(exhausted());
    }
    Ok(projected)
}

pub(super) fn links(links: &BTreeMap<String, Record>) -> Result<(), CoreError> {
    let mut total = usize::try_from(serialized_bytes(links)?).map_err(|_| exhausted())?;
    for record in links.values() {
        let current = usize::try_from(serialized_bytes(record)?).map_err(|_| exhausted())?;
        let mut growth = if record.outcome_digest.is_none() {
            OUTCOME_DIGEST
        } else {
            0
        };
        if record.seeds.is_some() && record.terminal_digests.is_none() {
            // Includes the field name, tuple punctuation and two 64-hex digests.
            growth = growth.checked_add(160).ok_or_else(exhausted)?;
        }
        if current
            .checked_add(growth)
            .is_none_or(|bytes| bytes > MAX_LINK_BYTES)
        {
            return Err(exhausted());
        }
        total = total.checked_add(growth).ok_or_else(exhausted)?;
    }
    if total > MAX_LINK_TOTAL {
        return Err(exhausted());
    }
    Ok(())
}

#[cfg(test)]
mod tests;
