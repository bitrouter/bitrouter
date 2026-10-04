//! Accounting evidence for individual managed provider attempts. Configured
//! token estimates are separate from authoritative invoices and cache evidence.

use serde::{Deserialize, Serialize};

use super::native::NativeAttemptReport;
use super::types::{ApiProtocol, NormalizedUsage, Usage, UsageOrigin};

/// Distinct observations of the same bill; never add these bases together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeCostBasis {
    /// Configured prices or another explicitly identified estimate.
    Estimated,
    /// Monetary amount explicitly reported by the source's billing authority.
    Reported,
    /// Report accepted by the host's existing reconciliation process.
    Reconciled,
}

/// What an amount covers, independently of execution work categories.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum NativeCostScope {
    /// Tokens only; does not establish provider tools, storage or other charges.
    ModelTokens,
    /// The source's logical request bill, not all work performed by a core run.
    RequestBill,
}

/// Content-free monetary evidence from a trusted host's existing settlement
/// store. The source/bill/basis tuple is immutable; changed evidence conflicts.
/// A source/bill identity belongs to one request across all evidence bases.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCostClaim {
    /// SDK request whose durable execution owns this observation.
    pub request_id: String,
    /// Stable host-controlled evidence source identifier.
    pub source: String,
    /// Stable billing identity within the source.
    pub bill_id: String,
    /// Estimate, report and reconciliation are separate observations.
    pub basis: NativeCostBasis,
    /// Explicit billing coverage.
    pub scope: NativeCostScope,
    /// Integer micro-USD, including authoritative zero when established.
    pub micro_usd: u64,
    /// Commitment to the retained source evidence, without exposing its content.
    pub evidence_sha256: String,
    /// Identity reported by this evidence, possibly absent on a no-charge bill.
    pub provider: String,
    /// Model identity reported by this evidence.
    pub model: String,
}

/// One source read for one caller-owned request.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCostObservation {
    /// Requested identity; missing and foreign records remain indistinguishable.
    pub request_id: String,
    /// Immutable, separately classified monetary evidence.
    pub claims: Vec<NativeCostClaim>,
    /// Missing, invisible, unreported or unreconciled amounts stay explicit.
    pub unknown_reason: Option<String>,
}

/// Read existing settlement evidence only. Implementations must authenticate
/// caller ownership, return one observation per requested ID, and never execute
/// generation, price usage again, write a charge or initiate reconciliation.
/// Reads must be cancellation safe; callers may drop a read on timeout.
#[async_trait::async_trait]
pub trait NativeCostSource: Send + Sync {
    /// Load at most 64 exact request identities owned by the caller.
    async fn read(
        &self,
        caller: &crate::caller::CallerContext,
        request_ids: &[String],
    ) -> crate::Result<Vec<NativeCostObservation>>;
}

/// Frozen rates used to reproduce a configured token estimate.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct NativeTokenRates {
    /// Micro-USD per uncached input token.
    pub uncached_input: Option<f64>,
    /// Micro-USD per cache-read input token.
    pub cache_read: Option<f64>,
    /// Micro-USD per cache-write input token.
    pub cache_write: Option<f64>,
    /// Micro-USD per output token, including reasoning.
    pub output: Option<f64>,
}

/// Lossless pricing floats in a bounded report-rejection summary.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct NativeTokenRateBits {
    /// Uncached-input rate as f64::to_bits.
    pub uncached_input: Option<u64>,
    /// Cache-read rate as f64::to_bits.
    pub cache_read: Option<u64>,
    /// Cache-write rate as f64::to_bits.
    pub cache_write: Option<u64>,
    /// Output rate as f64::to_bits.
    pub output: Option<u64>,
}

/// Model-token estimate only: excludes provider tools, storage, counter fees,
/// taxes and account-specific adjustments. It is not a final provider invoice.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum NativeTokenCost {
    /// The host has insufficient evidence to estimate this actual attempt.
    Unknown {
        /// Stable local reason, never upstream diagnostic text.
        reason: String,
    },
    /// The estimate remains unknown; its original diagnostic is committed.
    UnknownCommitment {
        /// Commitment to the original reason string or prior commitment.
        reason: super::native::NativeEvidenceCommitment,
    },
    /// The amount and numeric evidence remain available after metadata rejection.
    /// This is still an estimate, never a provider-reported or reconciled bill.
    ConfiguredEstimateCommitment {
        /// Host-estimated micro-USD, including an explicitly estimated zero.
        micro_usd: u64,
        /// Provenance of the counters used by the estimator.
        usage_origin: UsageOrigin,
        /// Non-overlapping input and output counters used for the estimate.
        normalized_usage: NormalizedUsage,
        /// Exact IEEE-754 bits, preserving non-finite evidence without JSON nulls.
        rates: NativeTokenRateBits,
        /// Commitment to original pricing identity fields or a prior commitment.
        pricing_metadata: super::native::NativeEvidenceCommitment,
    },
    /// The host applied a frozen configured price to canonical usage.
    ConfiguredEstimate {
        /// Rounded integer micro-USD from the host's shared metering calculation.
        micro_usd: u64,
        /// Origin of the canonical counters used by that calculation.
        usage_origin: UsageOrigin,
        /// Non-overlapping counters used for the estimate.
        normalized_usage: NormalizedUsage,
        /// Effective rates after context-tier inheritance.
        rates: NativeTokenRates,
        /// Digest identifying the complete pricing entry.
        pricing_version: String,
        /// Provider key used to resolve the price.
        pricing_provider: String,
        /// Served model key used to resolve the price.
        pricing_model: String,
    },
}

impl NativeTokenCost {
    /// Make an explicitly unavailable token estimate.
    pub fn unknown(reason: impl Into<String>) -> Self {
        Self::Unknown {
            reason: reason.into(),
        }
    }

    /// Known configured token estimate, never a claim of an authoritative bill.
    pub fn estimated_micro_usd(&self) -> Option<u64> {
        match self {
            Self::ConfiguredEstimate { micro_usd, .. }
            | Self::ConfiguredEstimateCommitment { micro_usd, .. } => Some(*micro_usd),
            Self::Unknown { .. } | Self::UnknownCommitment { .. } => None,
        }
    }
}

impl Default for NativeTokenCost {
    fn default() -> Self {
        Self::unknown("accounting_unavailable")
    }
}

/// Host-owned, local accounting using the same price snapshot as settlement.
/// This must not perform I/O, charge the caller, modify routing or repeat a
/// settlement write. It runs after every actual managed attempt, before its
/// outcome is checkpointed, including failed fallback attempts.
pub trait NativeCostEstimator: Send + Sync {
    /// Estimate token cost from this attempt's serving identity and evidence.
    fn estimate(&self, report: &NativeAttemptReport) -> NativeTokenCost;
}

/// Cache counters explicitly present in the serving protocol's raw usage.
/// Missing counters remain `None`, even when the canonical compatibility field
/// defaults to zero. These counters do not establish KV transfer or cost savings.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NativeCacheObservation {
    /// Raw observed cache-read tokens, including an explicit observed zero.
    pub read_tokens: Option<u64>,
    /// Raw observed cache-write tokens, including an explicit observed zero.
    pub write_tokens: Option<u64>,
    /// Controlled protocol/provenance label, or `unknown`.
    pub source: String,
    /// Stable reason when no reliable observation can be retained.
    pub unknown_reason: Option<String>,
}

impl Default for NativeCacheObservation {
    fn default() -> Self {
        Self::unknown("cache_usage_unavailable")
    }
}

impl NativeCacheObservation {
    fn unknown(reason: &str) -> Self {
        Self {
            read_tokens: None,
            write_tokens: None,
            source: "unknown".into(),
            unknown_reason: Some(reason.into()),
        }
    }

    /// Project explicit provider counters only after checking canonical bounds.
    pub fn capture(protocol: &ApiProtocol, usage: Option<&Usage>) -> Self {
        let Some(usage) = usage.filter(|usage| usage.origin == UsageOrigin::ProviderReported)
        else {
            return Self::unknown("provider_cache_usage_unavailable");
        };
        if usage.normalized_buckets().is_err() {
            return Self::unknown("invalid_usage_buckets");
        }
        let Some(raw) = usage.raw.as_deref() else {
            return Self::unknown("raw_usage_unavailable");
        };
        if let Err(reason) = validate_usage_containers(protocol, raw) {
            return Self::unknown(reason);
        }
        // Use serving wire schemas, never fields inferred from the requested model.
        // https://developers.openai.com/api/docs/guides/prompt-caching
        // https://platform.claude.com/docs/en/build-with-claude/prompt-caching
        // https://ai.google.dev/api/generate-content#UsageMetadata
        let (read_path, write_path, source) = match protocol {
            ApiProtocol::Responses => (
                "/input_tokens_details/cached_tokens",
                Some("/input_tokens_details/cache_write_tokens"),
                "responses_usage",
            ),
            ApiProtocol::ChatCompletions => (
                "/prompt_tokens_details/cached_tokens",
                Some("/prompt_tokens_details/cache_write_tokens"),
                "chat_completions_usage",
            ),
            ApiProtocol::Messages => (
                "/cache_read_input_tokens",
                Some("/cache_creation_input_tokens"),
                "messages_usage",
            ),
            ApiProtocol::GenerateContent => {
                ("/cachedContentTokenCount", None, "generate_content_usage")
            }
            _ => return Self::unknown("unsupported_cache_usage_protocol"),
        };
        let read = usage_field(protocol, raw, read_path);
        let write = write_path.and_then(|path| usage_field(protocol, raw, path));
        if read.is_some_and(|value| value.as_u64().is_none())
            || write.is_some_and(|value| value.as_u64().is_none())
        {
            return Self::unknown("invalid_raw_cache_counter");
        }
        let read_tokens = read.and_then(serde_json::Value::as_u64);
        let write_tokens = write.and_then(serde_json::Value::as_u64);
        if read_tokens.is_some_and(|value| value != usage.cache_read_tokens)
            || write_tokens.is_some_and(|value| value != usage.cache_write_tokens)
        {
            return Self::unknown("raw_cache_counter_mismatch");
        }
        if read_tokens.is_none() && write_tokens.is_none() {
            return Self::unknown("cache_counters_unreported");
        }
        Self {
            read_tokens,
            write_tokens,
            source: source.into(),
            unknown_reason: None,
        }
    }
}

// The Messages decoder retains the initial and cumulative terminal usage objects
// separately. A terminal explicit zero overrides the initial value; absence does
// not. Other serving protocols retain one flat provider usage object.
fn usage_field<'a>(
    protocol: &ApiProtocol,
    raw: &'a serde_json::Value,
    path: &str,
) -> Option<&'a serde_json::Value> {
    if *protocol == ApiProtocol::Messages
        && (raw.get("message_start").is_some() || raw.get("message_delta").is_some())
    {
        raw.get("message_delta")
            .and_then(|usage| usage.pointer(path))
            .or_else(|| {
                raw.get("message_start")
                    .and_then(|usage| usage.pointer(path))
            })
    } else {
        raw.pointer(path)
    }
}

fn validate_usage_containers(
    protocol: &ApiProtocol,
    raw: &serde_json::Value,
) -> Result<(), &'static str> {
    if !raw.is_object() {
        return Err("invalid_raw_usage_shape");
    }
    let containers: &[&str] = match protocol {
        ApiProtocol::Responses => &["input_tokens_details", "output_tokens_details"],
        ApiProtocol::ChatCompletions => &["prompt_tokens_details", "completion_tokens_details"],
        ApiProtocol::Messages => &["message_start", "message_delta"],
        _ => &[],
    };
    if containers
        .iter()
        .any(|name| raw.get(name).is_some_and(|value| !value.is_object()))
    {
        return Err("invalid_raw_usage_shape");
    }
    Ok(())
}

/// Verify complete provider token totals before applying configured prices.
/// Canonical defaults alone cannot distinguish an explicit zero from malformed
/// or omitted required counters. Optional cache/reasoning counters may be absent
/// only when their canonical value is zero. This accepts provider usage, not an
/// authoritative invoice or an in-process token estimate.
pub fn validated_provider_usage(
    protocol: &ApiProtocol,
    usage: &Usage,
) -> Result<NormalizedUsage, &'static str> {
    if usage.origin != UsageOrigin::ProviderReported {
        return Err("provider_usage_unavailable");
    }
    let normalized = usage
        .normalized_buckets()
        .map_err(|_| "invalid_usage_buckets")?;
    let raw = usage.raw.as_deref().ok_or("raw_usage_unavailable")?;
    validate_usage_containers(protocol, raw)?;
    // These are the serving schemas consumed by the SDK protocol decoders.
    // https://developers.openai.com/api/reference/typescript/resources/responses/methods/create
    // https://platform.claude.com/docs/en/api/messages
    // https://ai.google.dev/api/generate-content#UsageMetadata
    let (input_path, output_path, read_path, write_path, reasoning_path) = match protocol {
        ApiProtocol::Responses => (
            "/input_tokens",
            "/output_tokens",
            "/input_tokens_details/cached_tokens",
            Some("/input_tokens_details/cache_write_tokens"),
            Some("/output_tokens_details/reasoning_tokens"),
        ),
        ApiProtocol::ChatCompletions => (
            "/prompt_tokens",
            "/completion_tokens",
            "/prompt_tokens_details/cached_tokens",
            Some("/prompt_tokens_details/cache_write_tokens"),
            Some("/completion_tokens_details/reasoning_tokens"),
        ),
        ApiProtocol::Messages => (
            "/input_tokens",
            "/output_tokens",
            "/cache_read_input_tokens",
            Some("/cache_creation_input_tokens"),
            None,
        ),
        ApiProtocol::GenerateContent => (
            "/promptTokenCount",
            "/candidatesTokenCount",
            "/cachedContentTokenCount",
            None,
            Some("/thoughtsTokenCount"),
        ),
        _ => return Err("unsupported_usage_protocol"),
    };
    let required = |path| {
        // Initial output is a partial snapshot, unlike the input/cache fields.
        // A wrapped stream requires the terminal cumulative output counter.
        let value = if *protocol == ApiProtocol::Messages
            && path == "/output_tokens"
            && (raw.get("message_start").is_some() || raw.get("message_delta").is_some())
        {
            raw.get("message_delta")
                .and_then(|usage| usage.pointer(path))
        } else {
            usage_field(protocol, raw, path)
        };
        value
            .ok_or("required_usage_counter_missing")?
            .as_u64()
            .ok_or("invalid_raw_usage_counter")
    };
    let optional = |path: Option<&str>| -> Result<u64, &'static str> {
        path.and_then(|path| usage_field(protocol, raw, path))
            .map(|value| value.as_u64().ok_or("invalid_raw_usage_counter"))
            .transpose()
            .map(|value| value.unwrap_or(0))
    };
    let read = optional(Some(read_path))?;
    let write = optional(write_path)?;
    let reasoning = optional(reasoning_path)?;
    let input = required(input_path)?;
    let output = required(output_path)?;
    let input = if *protocol == ApiProtocol::Messages {
        input
            .checked_add(read)
            .and_then(|input| input.checked_add(write))
            .ok_or("raw_usage_counter_overflow")?
    } else {
        input
    };
    let output = if *protocol == ApiProtocol::GenerateContent {
        output
            .checked_add(reasoning)
            .ok_or("raw_usage_counter_overflow")?
    } else {
        output
    };
    if input != usage.prompt_tokens
        || output != usage.completion_tokens
        || read != usage.cache_read_tokens
        || write != usage.cache_write_tokens
        || reasoning != usage.reasoning_tokens
    {
        return Err("raw_usage_counter_mismatch");
    }
    Ok(normalized)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn usage(raw: serde_json::Value) -> Usage {
        Usage {
            prompt_tokens: 10,
            completion_tokens: 5,
            cache_read_tokens: 3,
            cache_write_tokens: 2,
            origin: UsageOrigin::ProviderReported,
            raw: Some(Box::new(raw)),
            ..Default::default()
        }
    }

    #[test]
    fn serving_protocols_validate_inclusive_totals_and_observed_cache() {
        for (protocol, raw, write, reasoning) in [
            (
                ApiProtocol::Responses,
                json!({"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":3,"cache_write_tokens":2}}),
                2,
                0,
            ),
            (
                ApiProtocol::ChatCompletions,
                json!({"prompt_tokens":10,"completion_tokens":5,"prompt_tokens_details":{"cached_tokens":3,"cache_write_tokens":2}}),
                2,
                0,
            ),
            (
                ApiProtocol::Messages,
                json!({"input_tokens":5,"output_tokens":5,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}),
                2,
                0,
            ),
            (
                ApiProtocol::GenerateContent,
                json!({"promptTokenCount":10,"candidatesTokenCount":3,"thoughtsTokenCount":2,"cachedContentTokenCount":3}),
                0,
                2,
            ),
        ] {
            let mut input = usage(raw);
            input.cache_write_tokens = write;
            input.reasoning_tokens = reasoning;
            assert_eq!(
                validated_provider_usage(&protocol, &input),
                input
                    .normalized_buckets()
                    .map_err(|_| "invalid_usage_buckets")
            );
            let cache = NativeCacheObservation::capture(&protocol, Some(&input));
            assert_eq!(cache.read_tokens, Some(3));
            assert_eq!(cache.write_tokens, (write == 2).then_some(2));
            assert!(cache.unknown_reason.is_none());
        }
    }

    #[test]
    fn messages_cumulative_delta_zero_overrides_initial_usage() {
        let mut input = usage(
            json!({"message_start":{"input_tokens":5,"output_tokens":0,"cache_read_input_tokens":3,"cache_creation_input_tokens":2},"message_delta":{"output_tokens":5,"cache_read_input_tokens":0}}),
        );
        input.prompt_tokens = 7;
        input.cache_read_tokens = 0;
        assert!(validated_provider_usage(&ApiProtocol::Messages, &input).is_ok());
        let cache = NativeCacheObservation::capture(&ApiProtocol::Messages, Some(&input));
        assert_eq!(cache.read_tokens, Some(0));
        assert_eq!(cache.write_tokens, Some(2));
        if let Some(raw) = input.raw.as_mut() {
            raw["message_delta"]["cache_read_input_tokens"] = json!(null);
        }
        assert!(validated_provider_usage(&ApiProtocol::Messages, &input).is_err());
        assert_eq!(
            NativeCacheObservation::capture(&ApiProtocol::Messages, Some(&input)).source,
            "unknown"
        );
    }

    #[test]
    fn cache_absence_is_distinct_from_observed_zero_and_estimates() {
        let mut input = usage(json!({"input_tokens":10,"output_tokens":5}));
        input.cache_read_tokens = 0;
        input.cache_write_tokens = 0;
        assert!(validated_provider_usage(&ApiProtocol::Responses, &input).is_ok());
        assert_eq!(
            NativeCacheObservation::capture(&ApiProtocol::Responses, Some(&input)).read_tokens,
            None
        );
        input.raw = Some(Box::new(
            json!({"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":0}}),
        ));
        assert_eq!(
            NativeCacheObservation::capture(&ApiProtocol::Responses, Some(&input)).read_tokens,
            Some(0)
        );
        for origin in [
            UsageOrigin::Estimated,
            UsageOrigin::Unknown,
            UsageOrigin::AuthoritativeReceipt,
        ] {
            input.origin = origin;
            assert!(validated_provider_usage(&ApiProtocol::Responses, &input).is_err());
            assert_eq!(
                NativeCacheObservation::capture(&ApiProtocol::Responses, Some(&input)).source,
                "unknown"
            );
        }
    }

    #[test]
    fn malformed_missing_or_mismatched_raw_counters_do_not_price() {
        for raw in [
            json!({"input_tokens":10}),
            json!({"input_tokens":10,"output_tokens":null}),
            json!({"input_tokens":"10","output_tokens":5}),
            json!({"input_tokens":11,"output_tokens":5}),
            json!({"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":-1}}),
            json!({"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":1}}),
        ] {
            assert!(validated_provider_usage(&ApiProtocol::Responses, &usage(raw)).is_err());
        }
        let mut input = usage(
            json!({"input_tokens":10,"output_tokens":5,"input_tokens_details":{"cached_tokens":30,"cache_write_tokens":2}}),
        );
        input.cache_read_tokens = 30;
        assert_eq!(
            NativeCacheObservation::capture(&ApiProtocol::Responses, Some(&input)).source,
            "unknown"
        );
        assert!(validated_provider_usage(&ApiProtocol::Responses, &input).is_err());
    }

    #[test]
    fn raw_counter_addition_cannot_saturate_into_valid_evidence() {
        let mut input = usage(
            json!({"input_tokens":u64::MAX,"output_tokens":5,"cache_read_input_tokens":3,"cache_creation_input_tokens":2}),
        );
        input.prompt_tokens = u64::MAX;
        assert_eq!(
            validated_provider_usage(&ApiProtocol::Messages, &input),
            Err("raw_usage_counter_overflow")
        );
        input.cache_write_tokens = 0;
        input.completion_tokens = u64::MAX;
        input.reasoning_tokens = 1;
        input.raw = Some(Box::new(
            json!({"promptTokenCount":u64::MAX,"candidatesTokenCount":u64::MAX,"thoughtsTokenCount":1,"cachedContentTokenCount":3}),
        ));
        assert_eq!(
            validated_provider_usage(&ApiProtocol::GenerateContent, &input),
            Err("raw_usage_counter_overflow")
        );
    }

    #[test]
    fn partial_messages_output_and_malformed_containers_cannot_prove_zero() {
        for raw in [
            json!({"message_start":{"input_tokens":10,"output_tokens":0}}),
            json!({"message_start":{"input_tokens":10,"output_tokens":0},"message_delta":{}}),
            json!({"message_start":{"input_tokens":10,"output_tokens":0},"message_delta":null}),
            json!({"message_start":null,"message_delta":{"input_tokens":10,"output_tokens":0}}),
        ] {
            let input = Usage {
                prompt_tokens: 10,
                origin: UsageOrigin::ProviderReported,
                raw: Some(Box::new(raw)),
                ..Default::default()
            };
            assert!(validated_provider_usage(&ApiProtocol::Messages, &input).is_err());
        }
        for malformed in [json!(null), json!(0), json!([])] {
            let input = Usage {
                prompt_tokens: 10,
                origin: UsageOrigin::ProviderReported,
                raw: Some(Box::new(
                    json!({"input_tokens":10,"output_tokens":0,"input_tokens_details":malformed}),
                )),
                ..Default::default()
            };
            assert_eq!(
                validated_provider_usage(&ApiProtocol::Responses, &input),
                Err("invalid_raw_usage_shape")
            );
            assert_eq!(
                NativeCacheObservation::capture(&ApiProtocol::Responses, Some(&input)).source,
                "unknown"
            );
        }
    }
}
