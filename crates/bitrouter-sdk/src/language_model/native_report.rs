//! Complete attempt-report admission. Rejection preserves commitments and
//! numeric evidence; original executor outcomes retain their SDK settlement.

use serde::Serialize;
use sha2::{Digest, Sha256};

use super::native::{
    NativeAttemptReport, NativeEvidenceCommitment, NativeReportRejection,
    NativeReportRejectionReason, NativeRoute,
};
use super::native_accounting::{NativeTokenCost, NativeTokenRateBits};
use crate::{BitrouterError, Result};

pub(super) fn rejection_byte_reserve(request_id: &str, route: &NativeRoute) -> Result<u64> {
    // Covers every fixed-schema field at maximum numeric width, two output
    // rejection summaries, three evidence commitments and pricing-rate bits.
    // Request/route bytes are counted verbatim, including their JSON escaping.
    commitment(&(request_id, route))?
        .bytes
        .checked_add(4096)
        .ok_or_else(|| BitrouterError::internal("attempt report envelope exhausted"))
}

pub(super) fn admit(report: &mut NativeAttemptReport, limit: u64) -> Result<bool> {
    let non_finite = non_finite_pricing(&report.token_cost);
    if !non_finite && super::native_output::fits(report, limit) {
        return Ok(false);
    }
    let original = commitment(report)?;
    let actual_provider = report
        .actual_provider
        .as_ref()
        .map(commitment)
        .transpose()?;
    let actual_model = report.actual_model.as_ref().map(commitment).transpose()?;
    let usage = report
        .result
        .as_ref()
        .and_then(|result| super::native_output::rejection(result, limit).usage)
        .or_else(|| {
            report
                .output_rejection
                .as_ref()
                .and_then(|rejected| rejected.usage.clone())
        });
    let had_result = report.result.is_some() || report.output_rejection.is_some();
    let token_cost = committed_cost(&report.token_cost)?;
    report.report_rejection = Some(NativeReportRejection {
        version: 1,
        byte_limit: limit,
        reason: if non_finite {
            NativeReportRejectionReason::NonFinitePricing
        } else {
            NativeReportRejectionReason::ByteLimit
        },
        original,
        actual_provider,
        actual_model,
        had_result,
        usage,
    });
    report.result = None;
    report.actual_provider = None;
    report.actual_model = None;
    report.token_cost = token_cost;
    report.error = report.rejection_reason().map(str::to_owned);
    if !super::native_output::fits(report, limit) {
        return Err(BitrouterError::internal(
            "attempt report rejection exceeds admitted envelope",
        ));
    }
    Ok(true)
}

fn non_finite_pricing(cost: &NativeTokenCost) -> bool {
    match cost {
        NativeTokenCost::ConfiguredEstimate { rates, .. } => [
            rates.uncached_input,
            rates.cache_read,
            rates.cache_write,
            rates.output,
        ]
        .into_iter()
        .flatten()
        .any(|rate| !rate.is_finite()),
        NativeTokenCost::ConfiguredEstimateCommitment { rates, .. } => [
            rates.uncached_input,
            rates.cache_read,
            rates.cache_write,
            rates.output,
        ]
        .into_iter()
        .flatten()
        .any(|rate| !f64::from_bits(rate).is_finite()),
        _ => false,
    }
}

fn committed_cost(cost: &NativeTokenCost) -> Result<NativeTokenCost> {
    Ok(match cost {
        NativeTokenCost::Unknown { reason } => NativeTokenCost::UnknownCommitment {
            reason: commitment(reason)?,
        },
        NativeTokenCost::UnknownCommitment { reason } => NativeTokenCost::UnknownCommitment {
            reason: commitment(reason)?,
        },
        NativeTokenCost::ConfiguredEstimate {
            micro_usd,
            usage_origin,
            normalized_usage,
            rates,
            pricing_version,
            pricing_provider,
            pricing_model,
        } => {
            #[derive(Serialize)]
            struct Pricing<'a> {
                pricing_version: &'a str,
                pricing_provider: &'a str,
                pricing_model: &'a str,
            }
            NativeTokenCost::ConfiguredEstimateCommitment {
                micro_usd: *micro_usd,
                usage_origin: *usage_origin,
                normalized_usage: *normalized_usage,
                rates: NativeTokenRateBits {
                    uncached_input: rates.uncached_input.map(f64::to_bits),
                    cache_read: rates.cache_read.map(f64::to_bits),
                    cache_write: rates.cache_write.map(f64::to_bits),
                    output: rates.output.map(f64::to_bits),
                },
                pricing_metadata: commitment(&Pricing {
                    pricing_version,
                    pricing_provider,
                    pricing_model,
                })?,
            }
        }
        NativeTokenCost::ConfiguredEstimateCommitment {
            micro_usd,
            usage_origin,
            normalized_usage,
            rates,
            pricing_metadata,
        } => NativeTokenCost::ConfiguredEstimateCommitment {
            micro_usd: *micro_usd,
            usage_origin: *usage_origin,
            normalized_usage: *normalized_usage,
            rates: rates.clone(),
            pricing_metadata: commitment(pricing_metadata)?,
        },
    })
}

pub(super) fn commitment(value: &impl Serialize) -> Result<NativeEvidenceCommitment> {
    let mut writer = HashCounter {
        bytes: 0,
        hash: Sha256::new(),
    };
    serde_json::to_writer(&mut writer, value)
        .map_err(|_| BitrouterError::internal("attempt report cannot be committed"))?;
    Ok(NativeEvidenceCommitment {
        bytes: writer.bytes,
        sha256: writer
            .hash
            .finalize()
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect(),
    })
}

struct HashCounter {
    bytes: u64,
    hash: Sha256,
}

impl std::io::Write for HashCounter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.bytes = self
            .bytes
            .checked_add(bytes.len() as u64)
            .ok_or_else(|| std::io::Error::other("attempt report commitment exhausted"))?;
        self.hash.update(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::native::{
        NativeOutputRejection, NativeOutputUsage, NativeRouteConstraints,
    };
    use crate::language_model::native_accounting::NativeTokenRates;
    use crate::language_model::native_context::{PrivateContextEvidence, PrivateContextFailure};
    use crate::language_model::native_continuation::{
        ContinuationFailure, NativeContinuationInput, NativeContinuationOutput,
    };
    use crate::language_model::types::{NormalizedUsage, UsageOrigin};
    use serde_json::json;

    fn report() -> Result<NativeAttemptReport> {
        serde_json::from_value(json!({"request_id":"request","attempt_index":0,
            "route":{"provider":"provider","model":"model","protocol":"chat_completions","constraints":NativeRouteConstraints::default()},
            "elapsed_ms":0}))
            .map_err(|error| BitrouterError::internal(error.to_string()))
    }

    #[test]
    fn exact_report_bound_preserves_content_and_one_more_byte_is_committed() -> Result<()> {
        let mut original = report()?;
        original.error = Some(String::new());
        let limit = rejection_byte_reserve(&original.request_id, &original.route)?;
        let padding = limit - commitment(&original)?.bytes;
        original.error = Some("x".repeat(padding as usize));
        assert_eq!(commitment(&original)?.bytes, limit);
        let mut retained = original.clone();
        assert!(!admit(&mut retained, limit)?);
        assert_eq!(retained, original);
        original
            .error
            .as_mut()
            .ok_or_else(|| BitrouterError::internal("fixture error missing"))?
            .push('x');
        let expected = commitment(&original)?;
        assert_eq!(expected.bytes, limit + 1);
        assert!(admit(&mut original, limit)?);
        assert_eq!(
            original
                .report_rejection
                .as_ref()
                .map(|rejected| &rejected.original),
            Some(&expected)
        );
        assert!(super::super::native_output::fits(&original, limit));
        assert!(matches!(
            original.token_cost,
            NativeTokenCost::UnknownCommitment { .. }
        ));
        Ok(())
    }

    #[test]
    fn maximal_rejection_evidence_fits_the_predispatch_envelope() -> Result<()> {
        for non_finite in [false, true] {
            for micro_usd in [0, u64::MAX] {
                let mut original = report()?;
                original.request_id = "\0\\\"request".repeat(1024);
                original.route.provider = "\0\\\"route".repeat(1024);
                original.attempt_index = u32::MAX;
                original.elapsed_ms = u64::MAX;
                original.actual_provider = Some("\0\\\"actual".repeat(4096));
                original.actual_model = Some("\0\\\"model".repeat(4096));
                original.cache.read_tokens = Some(u64::MAX);
                original.cache.write_tokens = Some(u64::MAX);
                // SDK-generated cache labels/reasons are controlled strings;
                // 128 bytes each exceeds every serving adapter's vocabulary.
                original.cache.source = "x".repeat(128);
                original.cache.unknown_reason = Some("x".repeat(128));
                original.private_context.input =
                    PrivateContextEvidence::Verified { parts: u64::MAX };
                original.private_context.output = PrivateContextEvidence::Unverified {
                    reason: PrivateContextFailure::AuthorityUnavailable,
                };
                original.continuation.input = NativeContinuationInput::Resumed {
                    prefix_messages: u64::MAX,
                };
                original.continuation.output = NativeContinuationOutput::Unverified {
                    reason: ContinuationFailure::AuthorityUnavailable,
                };
                original.output_rejection = Some(NativeOutputRejection {
                    byte_limit: u64::MAX,
                    usage: Some(NativeOutputUsage {
                        prompt_tokens: u64::MAX,
                        completion_tokens: u64::MAX,
                        reasoning_tokens: u64::MAX,
                        cache_read_tokens: u64::MAX,
                        cache_write_tokens: u64::MAX,
                        web_search_count: u64::MAX,
                        origin: UsageOrigin::AuthoritativeReceipt,
                    }),
                });
                original.token_cost = NativeTokenCost::ConfiguredEstimate {
                    micro_usd,
                    usage_origin: UsageOrigin::AuthoritativeReceipt,
                    normalized_usage: NormalizedUsage {
                        uncached_input_tokens: u64::MAX,
                        cache_read_tokens: u64::MAX,
                        cache_write_tokens: u64::MAX,
                        output_tokens: u64::MAX,
                        reasoning_tokens: u64::MAX,
                    },
                    rates: NativeTokenRates {
                        uncached_input: Some(if non_finite { f64::NAN } else { f64::MAX }),
                        cache_read: Some(f64::MIN),
                        cache_write: Some(-0.0),
                        output: Some(f64::MIN_POSITIVE),
                    },
                    pricing_version: "\0price".repeat(4096),
                    pricing_provider: "\0provider".repeat(4096),
                    pricing_model: "\0model".repeat(4096),
                };
                let envelope = rejection_byte_reserve(&original.request_id, &original.route)?;
                let expected_provider = original
                    .actual_provider
                    .as_ref()
                    .map(commitment)
                    .transpose()?;
                assert!(admit(&mut original, envelope)?);
                assert_eq!(original.token_cost.estimated_micro_usd(), Some(micro_usd));
                let rejected = original
                    .report_rejection
                    .as_mut()
                    .ok_or_else(|| BitrouterError::internal("rejection missing"))?;
                assert_eq!(rejected.actual_provider, expected_provider);
                assert_eq!(
                    rejected.reason,
                    if non_finite {
                        NativeReportRejectionReason::NonFinitePricing
                    } else {
                        NativeReportRejectionReason::ByteLimit
                    }
                );
                // Stress all numeric widths in the envelope, independently of
                // this fixture's particular report length or allowance.
                rejected.byte_limit = u64::MAX;
                rejected.original.bytes = u64::MAX;
                if let Some(value) = &mut rejected.actual_provider {
                    value.bytes = u64::MAX;
                }
                if let Some(value) = &mut rejected.actual_model {
                    value.bytes = u64::MAX;
                }
                assert!(super::super::native_output::fits(&original, envelope));
                let encoded = serde_json::to_vec(&original)
                    .map_err(|error| BitrouterError::internal(error.to_string()))?;
                let round_trip: NativeAttemptReport = serde_json::from_slice(&encoded)
                    .map_err(|error| BitrouterError::internal(error.to_string()))?;
                assert_eq!(round_trip, original);
                let NativeTokenCost::ConfiguredEstimateCommitment { rates, .. } =
                    &original.token_cost
                else {
                    return Err(BitrouterError::internal("estimate missing"));
                };
                assert_eq!(
                    rates.uncached_input,
                    Some(if non_finite {
                        f64::NAN.to_bits()
                    } else {
                        f64::MAX.to_bits()
                    })
                );
            }
        }
        Ok(())
    }

    #[test]
    fn non_finite_pricing_is_committed_even_when_the_report_fits() -> Result<()> {
        let mut report = report()?;
        report.token_cost = NativeTokenCost::ConfiguredEstimate {
            micro_usd: 7,
            usage_origin: UsageOrigin::Estimated,
            normalized_usage: NormalizedUsage::default(),
            rates: NativeTokenRates {
                uncached_input: Some(f64::INFINITY),
                cache_read: None,
                cache_write: None,
                output: None,
            },
            pricing_version: "v1".into(),
            pricing_provider: "provider".into(),
            pricing_model: "model".into(),
        };
        let limit = rejection_byte_reserve(&report.request_id, &report.route)?;
        assert!(super::super::native_output::fits(&report, limit));
        assert!(admit(&mut report, limit)?);
        assert_eq!(report.token_cost.estimated_micro_usd(), Some(7));
        assert_eq!(
            report
                .report_rejection
                .as_ref()
                .map(|rejected| rejected.reason),
            Some(NativeReportRejectionReason::NonFinitePricing)
        );
        Ok(())
    }
}
