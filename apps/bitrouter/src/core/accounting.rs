//! Model-token estimates accumulated across the whole root run, including
//! retired child turns and fallbacks. Provider invoices and auxiliary charges
//! are outside this subtotal; it must not be added to request settlement rows.

use crate::storage::checkpoint::snapshot::accounting::RunTokenAccounting;

pub(crate) mod claims;
pub mod work;

use bitrouter_ai::core::evidence::NativeTokenCost;

impl RunTokenAccounting {
    /// Called only while recording the first outcome for a committed attempt ID,
    /// in the same checkpoint mutation as its receipt. Run admission bounds the
    /// number of outcomes by a u32 count, including child turns and fallbacks.
    pub(crate) fn record(&mut self, cost: &NativeTokenCost) {
        match cost.estimated_micro_usd() {
            Some(amount) => {
                self.known_attempts += 1;
                self.known_subtotal_micro_usd = self
                    .known_subtotal_micro_usd
                    .and_then(|sum| sum.checked_add(amount));
            }
            None => self.unknown_attempts += 1,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_ai::core::evidence::NativeTokenRates;

    #[test]
    fn run_subtotal_overflow_stays_unknown_after_later_receipts() {
        let mut accounting = RunTokenAccounting::default();
        let cost = NativeTokenCost::ConfiguredEstimate {
            micro_usd: i64::MAX as u64,
            usage_origin: Default::default(),
            normalized_usage: Default::default(),
            rates: NativeTokenRates {
                uncached_input: None,
                cache_read: None,
                cache_write: None,
                output: None,
            },
            pricing_version: "fixture".into(),
            pricing_provider: "fixture".into(),
            pricing_model: "fixture".into(),
        };
        for _ in 0..3 {
            accounting.record(&cost);
        }
        assert_eq!(accounting.known_subtotal_micro_usd, None);
        assert_eq!(accounting.complete_estimate_micro_usd(3), None);
        accounting.record(&NativeTokenCost::unknown("missing_receipt"));
        assert_eq!(accounting.known_attempts, 3);
        assert_eq!(accounting.unknown_attempts, 1);
        assert_eq!(accounting.known_subtotal_micro_usd, None);
    }
}
