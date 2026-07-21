//! Model pricing and charge calculation.
//!
//! v0 bug defences baked in here:
//! - **#180** — a default (all-zero) `ModelPricing` is treated as *unconfigured*,
//!   not as *free*: settlement records an explicit unknown charge rather than
//!   silently treating the request as zero cost.
//! - **#440 → #441** — pricing lookup covers every level; a `(provider, model)`
//!   miss is reported, not papered over.
//! - **#443 → #445** — the lookup is keyed by `(provider, service_id)` so a
//!   service id that differs from the public model name still resolves.

use std::collections::HashMap;

use bitrouter_sdk::language_model::{NormalizedUsage, Usage, UsageNormalizationError};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};

/// Per-model pricing, in **micro-USD per token**.
///
/// Each rate is `Option<f64>` so a *partially-configured* entry — e.g. input
/// rate set, output rate omitted — can be distinguished from one that has
/// been deliberately set to zero. Previously both were `0.0` and the charge
/// math silently undercharged every output token for the partially-priced
/// case (v0 bitrouter#463 / cloud#251 audit B4).
///
/// Defences:
/// - **#180** — a default (all-`None`) `ModelPricing` is treated as
///   *unconfigured*: settlement records an explicit unknown charge rather
///   than silently treating the request as zero cost.
/// - **bitrouter#463-A** — if any token bucket with nonzero usage lacks a
///   rate, `calculate_charge_micro_usd` returns `None` and the charge is
///   skipped. A model with input rate set but `output_micro_usd_per_token`
///   missing no longer bills every output token at $0.
/// - **#440 → #441** — pricing lookup covers every level; a `(provider,
///   model)` miss is reported, not papered over.
/// - **#443 → #445** — the lookup is keyed by `(provider, service_id)` so a
///   service id that differs from the public model name still resolves.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ModelPricing {
    /// Micro-USD charged per prompt (input) token (base bracket). `None` =
    /// unconfigured.
    pub input_micro_usd_per_token: Option<f64>,
    /// Micro-USD charged per cache-read input token. `None` = unconfigured.
    pub cache_read_micro_usd_per_token: Option<f64>,
    /// Micro-USD charged per cache-write input token. `None` = unconfigured.
    pub cache_write_micro_usd_per_token: Option<f64>,
    /// Micro-USD charged per completion (output) token (base bracket).
    /// `None` = unconfigured.
    pub output_micro_usd_per_token: Option<f64>,
    /// Optional higher context brackets, applied by total input-token count.
    /// Empty ⇒ flat pricing. See [`ContextTier`] and
    /// [`resolve_for_input_tokens`](Self::resolve_for_input_tokens).
    pub context_tiers: Vec<ContextTier>,
}

/// A higher context-pricing bracket: a steeper per-token rate that applies
/// once a request's input (prompt) token count crosses
/// [`above_input_tokens`](Self::above_input_tokens). The selected bracket's
/// rates apply to the whole request (a step function, not graduated marginal
/// brackets). Mirrors the base [`ModelPricing`] rate fields.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ContextTier {
    /// Exclusive lower bound on total input tokens. A request whose input
    /// size is strictly greater than this enters the bracket; one exactly at
    /// the bound stays in the lower bracket.
    pub above_input_tokens: u64,
    /// Micro-USD per prompt (input) token for this bracket.
    pub input_micro_usd_per_token: Option<f64>,
    /// Micro-USD per cache-read input token for this bracket. `None` inherits
    /// the base rate.
    pub cache_read_micro_usd_per_token: Option<f64>,
    /// Micro-USD per cache-write input token for this bracket. `None` inherits
    /// the base rate.
    pub cache_write_micro_usd_per_token: Option<f64>,
    /// Micro-USD per completion (output) token for this bracket.
    pub output_micro_usd_per_token: Option<f64>,
}

impl ModelPricing {
    /// Build a pricing entry with both base rates set and no context tiers.
    /// Use [`partial`](Self::partial) when only some rates are known.
    pub fn new(input_micro_usd_per_token: f64, output_micro_usd_per_token: f64) -> Self {
        Self {
            input_micro_usd_per_token: Some(input_micro_usd_per_token),
            cache_read_micro_usd_per_token: None,
            cache_write_micro_usd_per_token: None,
            output_micro_usd_per_token: Some(output_micro_usd_per_token),
            context_tiers: Vec::new(),
        }
    }

    /// Build a partial pricing entry. Buckets with nonzero usage but no rate
    /// are billed as `None` (charge is skipped).
    pub fn partial(input: Option<f64>, output: Option<f64>) -> Self {
        Self {
            input_micro_usd_per_token: input,
            cache_read_micro_usd_per_token: None,
            cache_write_micro_usd_per_token: None,
            output_micro_usd_per_token: output,
            context_tiers: Vec::new(),
        }
    }

    /// Build cache-aware pricing. Each rate remains optional so a missing
    /// provider rate is distinguishable from an explicitly free bucket.
    pub fn cache_aware(
        input: Option<f64>,
        cache_read: Option<f64>,
        cache_write: Option<f64>,
        output: Option<f64>,
    ) -> Self {
        Self {
            input_micro_usd_per_token: input,
            cache_read_micro_usd_per_token: cache_read,
            cache_write_micro_usd_per_token: cache_write,
            output_micro_usd_per_token: output,
            context_tiers: Vec::new(),
        }
    }

    /// Resolve the effective bracket for a request with `input_tokens` total
    /// prompt tokens: the `context_tiers` entry with the greatest
    /// `above_input_tokens` strictly below `input_tokens`, or the base rates
    /// when none qualifies. Returns flat pricing (no tiers); order-independent.
    pub fn resolve_for_input_tokens(&self, input_tokens: u64) -> ModelPricing {
        match self
            .context_tiers
            .iter()
            .filter(|t| input_tokens > t.above_input_tokens)
            .max_by_key(|t| t.above_input_tokens)
        {
            Some(tier) => ModelPricing {
                input_micro_usd_per_token: tier
                    .input_micro_usd_per_token
                    .or(self.input_micro_usd_per_token),
                cache_read_micro_usd_per_token: tier
                    .cache_read_micro_usd_per_token
                    .or(self.cache_read_micro_usd_per_token),
                cache_write_micro_usd_per_token: tier
                    .cache_write_micro_usd_per_token
                    .or(self.cache_write_micro_usd_per_token),
                output_micro_usd_per_token: tier
                    .output_micro_usd_per_token
                    .or(self.output_micro_usd_per_token),
                context_tiers: Vec::new(),
            },
            None => ModelPricing {
                input_micro_usd_per_token: self.input_micro_usd_per_token,
                cache_read_micro_usd_per_token: self.cache_read_micro_usd_per_token,
                cache_write_micro_usd_per_token: self.cache_write_micro_usd_per_token,
                output_micro_usd_per_token: self.output_micro_usd_per_token,
                context_tiers: Vec::new(),
            },
        }
    }

    /// Whether this pricing has no rates set at all — i.e. **unconfigured**.
    /// Distinct from `partial` (some rates set, some not) and from
    /// explicitly-zero rates.
    pub fn is_unconfigured(&self) -> bool {
        self.input_micro_usd_per_token.is_none()
            && self.cache_read_micro_usd_per_token.is_none()
            && self.cache_write_micro_usd_per_token.is_none()
            && self.output_micro_usd_per_token.is_none()
    }

    /// Whether *every* rate this struct exposes is set. Useful for a
    /// recommender / "cheapest" picker that should refuse placeholder
    /// entries (v0 bitrouter#463 / cloud#251 audit B5).
    pub fn is_complete(&self) -> bool {
        self.input_micro_usd_per_token.is_some()
            && self.cache_read_micro_usd_per_token.is_some()
            && self.cache_write_micro_usd_per_token.is_some()
            && self.output_micro_usd_per_token.is_some()
    }
}

/// Whether a charge was computed from complete evidence.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChargeStatus {
    /// All nonzero token buckets had valid rates.
    Computed,
    /// An authoritative receipt confirms that the request was not charged.
    NotCharged,
    /// Usage, normalization, or pricing evidence was incomplete.
    Unknown,
    /// A pre-evidence row retained from an older database schema.
    #[default]
    LegacyUnknown,
}

impl ChargeStatus {
    /// Stable database and JSON value.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Computed => "computed",
            Self::NotCharged => "not_charged",
            Self::Unknown => "unknown",
            Self::LegacyUnknown => "legacy_unknown",
        }
    }

    /// Parse a persisted status, treating unrecognized historical values as
    /// legacy unknown rather than computed.
    pub fn from_persisted(value: &str) -> Self {
        match value {
            "computed" => Self::Computed,
            "not_charged" => Self::NotCharged,
            "unknown" => Self::Unknown,
            _ => Self::LegacyUnknown,
        }
    }
}

/// Origin of the rates used for settlement evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PricingSource {
    /// Rates came from the effective provider/model configuration.
    Configured,
    /// Rates came from an explicit benchmark/export override.
    Override,
    /// No usable pricing entry was found.
    Unknown,
}

/// Effective rates after context-tier inheritance.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct EffectivePricingRates {
    /// Uncached input rate.
    pub uncached_input_micro_usd_per_token: Option<f64>,
    /// Cache-read input rate.
    pub cache_read_micro_usd_per_token: Option<f64>,
    /// Cache-write input rate.
    pub cache_write_micro_usd_per_token: Option<f64>,
    /// Output rate, applied to text and reasoning output.
    pub output_micro_usd_per_token: Option<f64>,
}

/// Auditable result of normalizing usage and applying effective pricing.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ChargeEvidence {
    /// Whether the final charge is usable.
    pub status: ChargeStatus,
    /// Computed charge, absent when status is not `computed`.
    pub charge_micro_usd: Option<i64>,
    /// Non-overlapping usage buckets used by the calculation.
    pub normalized_usage: NormalizedUsage,
    /// Effective per-token rates.
    pub effective_rates: EffectivePricingRates,
    /// Where those rates came from.
    pub pricing_source: PricingSource,
    /// Deterministic digest of the full pricing entry.
    pub pricing_version: String,
    /// Stable reason when a charge could not be computed.
    pub unknown_reason: Option<String>,
}

/// A `(provider, service_id)` → `ModelPricing` table. In Phase 4 this is built
/// from the registry-style provider config; for Phase 3 it is constructed
/// directly (and by tests).
#[derive(Debug, Clone, Default)]
pub struct PricingTable {
    entries: HashMap<(String, String), ModelPricing>,
}

impl PricingTable {
    /// An empty table.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register pricing for `(provider, service_id)`.
    pub fn insert(
        &mut self,
        provider: impl Into<String>,
        service_id: impl Into<String>,
        pricing: ModelPricing,
    ) {
        self.entries
            .insert((provider.into(), service_id.into()), pricing);
    }

    /// Resolve pricing for a `(provider, service_id)` pair. Returns `None` when
    /// no entry exists — the caller must treat that as "unconfigured", not free
    /// (#180 / #440 / #443).
    ///
    /// Called on every charge / streaming usage event, so it avoids
    /// `provider.to_string() + service_id.to_string()` (a fresh allocation
    /// per lookup): a 2-tuple of `&str` hashes the same as `(String, String)`
    /// under the standard `Hash` derivation, so `HashMap::get` with the
    /// `BorrowedKey` newtype reuses the borrow.
    pub fn resolve(&self, provider: &str, service_id: &str) -> Option<ModelPricing> {
        self.entries
            .get(&BorrowedKey(provider, service_id) as &dyn KeyLike)
            .cloned()
    }
}

/// Newtype wrapping `(&str, &str)` so `HashMap<(String, String), _>::get`
/// can take a borrowed key without allocating two `String`s per lookup.
/// We use a small trait-object trick: the map's key (`(String, String)`)
/// and `BorrowedKey<'_>` both implement `KeyLike`, which `Hash + Eq`-equals
/// the pair so the map can find the entry by reference.
struct BorrowedKey<'a>(&'a str, &'a str);

trait KeyLike {
    fn parts(&self) -> (&str, &str);
}

impl KeyLike for (String, String) {
    fn parts(&self) -> (&str, &str) {
        (self.0.as_str(), self.1.as_str())
    }
}

impl KeyLike for BorrowedKey<'_> {
    fn parts(&self) -> (&str, &str) {
        (self.0, self.1)
    }
}

impl std::hash::Hash for dyn KeyLike + '_ {
    fn hash<H: std::hash::Hasher>(&self, state: &mut H) {
        let (a, b) = self.parts();
        a.hash(state);
        b.hash(state);
    }
}

impl PartialEq for dyn KeyLike + '_ {
    fn eq(&self, other: &Self) -> bool {
        self.parts() == other.parts()
    }
}

impl Eq for dyn KeyLike + '_ {}

impl<'a> std::borrow::Borrow<dyn KeyLike + 'a> for (String, String) {
    fn borrow(&self) -> &(dyn KeyLike + 'a) {
        self
    }
}

/// Maximum trusted upstream-reported token count per bucket. Clamps the
/// charge math against an adversarial upstream returning `u64::MAX` tokens
/// (v0 bitrouter#463 / cloud#251 audit B9). A real provider response has at
/// most a few hundred-thousand tokens; this cap is two orders of magnitude
/// above any plausible real value.
pub const MAX_TRUSTED_TOKENS: u64 = 10_000_000;

/// Compute the charge for a request, in micro-USD, from its usage and pricing.
///
/// Returns `None` when any token bucket with nonzero usage lacks a rate —
/// the caller MUST treat that as "pricing unavailable" and skip the charge
/// (never bill the zero). Returns `Some(0)` when all buckets are zero
/// (genuinely no work to bill).
///
/// Each bucket's token count is clamped to [`MAX_TRUSTED_TOKENS`] before the
/// math; an adversarial upstream can't drive the charge to overflow.
/// Result rounds to the nearest whole micro-USD; never negative.
///
/// Context-tier pricing is resolved here: the prompt-token count selects the
/// bracket (see [`ModelPricing::resolve_for_input_tokens`]) and its rates bill
/// the whole request. Flat pricing (no tiers) resolves to its base rates.
pub fn calculate_charge_micro_usd(usage: &Usage, pricing: &ModelPricing) -> Option<i64> {
    calculate_charge_evidence(usage, pricing, PricingSource::Configured).charge_micro_usd
}

/// Normalize inclusive provider totals and calculate a four-bucket charge with
/// sufficient evidence to audit the result later.
pub fn calculate_charge_evidence(
    usage: &Usage,
    pricing: &ModelPricing,
    pricing_source: PricingSource,
) -> ChargeEvidence {
    let pricing_version = pricing_version(pricing);
    let resolved = pricing.resolve_for_input_tokens(usage.prompt_tokens);
    let effective_rates = EffectivePricingRates {
        uncached_input_micro_usd_per_token: resolved.input_micro_usd_per_token,
        cache_read_micro_usd_per_token: resolved.cache_read_micro_usd_per_token,
        cache_write_micro_usd_per_token: resolved.cache_write_micro_usd_per_token,
        output_micro_usd_per_token: resolved.output_micro_usd_per_token,
    };
    let normalized_usage = match usage.normalized_buckets() {
        Ok(normalized) => normalized,
        Err(error) => {
            let reason = match error {
                UsageNormalizationError::InputBucketsOverlap => "invalid_input_buckets",
                UsageNormalizationError::OutputBucketsOverlap => "invalid_output_buckets",
            };
            return unknown_evidence(
                NormalizedUsage::default(),
                effective_rates,
                pricing_source,
                pricing_version,
                reason,
            );
        }
    };

    let buckets = [
        (
            normalized_usage.uncached_input_tokens,
            effective_rates.uncached_input_micro_usd_per_token,
            "missing_uncached_input_rate",
            "invalid_uncached_input_rate",
        ),
        (
            normalized_usage.cache_read_tokens,
            effective_rates.cache_read_micro_usd_per_token,
            "missing_cache_read_rate",
            "invalid_cache_read_rate",
        ),
        (
            normalized_usage.cache_write_tokens,
            effective_rates.cache_write_micro_usd_per_token,
            "missing_cache_write_rate",
            "invalid_cache_write_rate",
        ),
    ];
    let mut charge = 0.0;
    for (tokens, rate, missing_reason, invalid_reason) in buckets {
        if tokens > 0 && rate.is_some_and(|value| !value.is_finite() || value < 0.0) {
            return unknown_evidence(
                normalized_usage,
                effective_rates,
                pricing_source,
                pricing_version,
                invalid_reason,
            );
        }
        let Some(bucket) = bucket_charge(tokens, rate) else {
            return unknown_evidence(
                normalized_usage,
                effective_rates,
                pricing_source,
                pricing_version,
                missing_reason,
            );
        };
        charge += bucket;
    }

    // Output and reasoning tokens are charged at the same rate. Combine their
    // individually clamped counts before multiplication so a half-micro-USD
    // boundary is rounded exactly once, matching provider receipt semantics.
    let output_tokens = normalized_usage
        .output_tokens
        .min(MAX_TRUSTED_TOKENS)
        .saturating_add(normalized_usage.reasoning_tokens.min(MAX_TRUSTED_TOKENS));
    let output_rate = effective_rates.output_micro_usd_per_token;
    if output_tokens > 0 && output_rate.is_some_and(|value| !value.is_finite() || value < 0.0) {
        return unknown_evidence(
            normalized_usage,
            effective_rates,
            pricing_source,
            pricing_version,
            "invalid_output_rate",
        );
    }
    let Some(output_charge) = charge_for_tokens(output_tokens, output_rate) else {
        return unknown_evidence(
            normalized_usage,
            effective_rates,
            pricing_source,
            pricing_version,
            "missing_output_rate",
        );
    };
    charge += output_charge;

    ChargeEvidence {
        status: ChargeStatus::Computed,
        charge_micro_usd: Some(charge.round().max(0.0) as i64),
        normalized_usage,
        effective_rates,
        pricing_source,
        pricing_version,
        unknown_reason: None,
    }
}

/// Evidence for a request with no usable pricing entry.
pub fn unavailable_charge_evidence(usage: &Usage, reason: &str) -> ChargeEvidence {
    let normalized = usage.normalized_buckets().unwrap_or_default();
    unknown_evidence(
        normalized,
        EffectivePricingRates::default(),
        PricingSource::Unknown,
        "sha256:unavailable".to_string(),
        reason,
    )
}

fn unknown_evidence(
    normalized_usage: NormalizedUsage,
    effective_rates: EffectivePricingRates,
    pricing_source: PricingSource,
    pricing_version: String,
    reason: &str,
) -> ChargeEvidence {
    ChargeEvidence {
        status: ChargeStatus::Unknown,
        charge_micro_usd: None,
        normalized_usage,
        effective_rates,
        pricing_source,
        pricing_version,
        unknown_reason: Some(reason.to_string()),
    }
}

fn pricing_version(pricing: &ModelPricing) -> String {
    let mut hasher = Sha256::new();
    hash_rate(&mut hasher, pricing.input_micro_usd_per_token);
    hash_rate(&mut hasher, pricing.cache_read_micro_usd_per_token);
    hash_rate(&mut hasher, pricing.cache_write_micro_usd_per_token);
    hash_rate(&mut hasher, pricing.output_micro_usd_per_token);
    let mut tiers = pricing.context_tiers.clone();
    tiers.sort_by_key(|tier| tier.above_input_tokens);
    for tier in tiers {
        hasher.update(tier.above_input_tokens.to_be_bytes());
        hash_rate(&mut hasher, tier.input_micro_usd_per_token);
        hash_rate(&mut hasher, tier.cache_read_micro_usd_per_token);
        hash_rate(&mut hasher, tier.cache_write_micro_usd_per_token);
        hash_rate(&mut hasher, tier.output_micro_usd_per_token);
    }
    format!("sha256:{}", hex::encode(hasher.finalize()))
}

fn hash_rate(hasher: &mut Sha256, rate: Option<f64>) {
    match rate {
        Some(value) => {
            hasher.update([1]);
            hasher.update(value.to_bits().to_be_bytes());
        }
        None => hasher.update([0]),
    }
}

/// One token bucket's contribution to the charge. `Some(0.0)` when the bucket
/// is zero (no work to bill, rate may legitimately be missing); `None` when
/// the bucket has nonzero usage but no rate.
fn bucket_charge(tokens: u64, rate: Option<f64>) -> Option<f64> {
    charge_for_tokens(tokens.min(MAX_TRUSTED_TOKENS), rate)
}

/// Calculate one contribution from an already bounded token count.
fn charge_for_tokens(tokens: u64, rate: Option<f64>) -> Option<f64> {
    match (tokens, rate) {
        (0, _) => Some(0.0),
        (n, Some(r)) => Some(n as f64 * r),
        (_, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::language_model::UsageOrigin;

    #[test]
    fn default_pricing_reads_as_unconfigured() {
        assert!(ModelPricing::default().is_unconfigured());
        assert!(!ModelPricing::new(1.0, 2.0).is_unconfigured());
    }

    #[test]
    fn charge_is_input_plus_output() {
        let pricing = ModelPricing::new(2.0, 10.0);
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 50,
            reasoning_tokens: 0,
            ..Default::default()
        };
        // 100*2 + 50*10 = 700
        assert_eq!(calculate_charge_micro_usd(&usage, &pricing), Some(700));
    }

    #[test]
    fn cache_aware_charge_uses_four_non_overlapping_buckets() {
        let pricing = ModelPricing::cache_aware(Some(2.0), Some(0.2), Some(2.5), Some(10.0));
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 30,
            reasoning_tokens: 10,
            cache_read_tokens: 40,
            cache_write_tokens: 20,
            origin: UsageOrigin::ProviderReported,
            ..Default::default()
        };

        let evidence = calculate_charge_evidence(&usage, &pricing, PricingSource::Configured);

        assert_eq!(evidence.status, ChargeStatus::Computed);
        assert_eq!(evidence.charge_micro_usd, Some(438));
        assert_eq!(evidence.normalized_usage.uncached_input_tokens, 40);
        assert_eq!(evidence.normalized_usage.cache_read_tokens, 40);
        assert_eq!(evidence.normalized_usage.cache_write_tokens, 20);
        assert_eq!(evidence.normalized_usage.output_tokens, 20);
        assert_eq!(evidence.normalized_usage.reasoning_tokens, 10);
        assert_eq!(evidence.pricing_source, PricingSource::Configured);
        assert!(evidence.pricing_version.starts_with("sha256:"));
    }

    /// Regression for full89 Sol r2: output and reasoning share one provider
    /// rate, so their tokens must be combined before rounding the final charge.
    /// Multiplying the two buckets separately can move an exact half-micro-USD
    /// boundary below 1984.5 due to binary floating-point accumulation.
    #[test]
    fn shared_output_rate_rounds_after_combining_completion_tokens() {
        let pricing = ModelPricing::cache_aware(Some(0.84), Some(0.84), Some(0.84), Some(3.99));
        let usage = Usage {
            prompt_tokens: 1_137,
            completion_tokens: 258,
            reasoning_tokens: 45,
            origin: UsageOrigin::ProviderReported,
            ..Default::default()
        };

        assert_eq!(calculate_charge_micro_usd(&usage, &pricing), Some(1_985));
    }

    #[test]
    fn missing_cache_rate_keeps_charge_unknown() {
        let pricing = ModelPricing::cache_aware(Some(2.0), None, Some(2.5), Some(10.0));
        let usage = Usage {
            prompt_tokens: 100,
            cache_read_tokens: 40,
            completion_tokens: 1,
            ..Default::default()
        };

        let evidence = calculate_charge_evidence(&usage, &pricing, PricingSource::Configured);

        assert_eq!(evidence.status, ChargeStatus::Unknown);
        assert_eq!(evidence.charge_micro_usd, None);
        assert_eq!(
            evidence.unknown_reason.as_deref(),
            Some("missing_cache_read_rate")
        );
    }

    #[test]
    fn invalid_nonzero_bucket_rate_keeps_charge_unknown() {
        for (rate, reason) in [
            (f64::NAN, "invalid_uncached_input_rate"),
            (f64::INFINITY, "invalid_uncached_input_rate"),
            (-1.0, "invalid_uncached_input_rate"),
        ] {
            let pricing = ModelPricing::cache_aware(Some(rate), Some(0.2), Some(2.5), Some(10.0));
            let usage = Usage {
                prompt_tokens: 1,
                ..Default::default()
            };

            let evidence = calculate_charge_evidence(&usage, &pricing, PricingSource::Configured);

            assert_eq!(evidence.status, ChargeStatus::Unknown);
            assert_eq!(evidence.charge_micro_usd, None);
            assert_eq!(evidence.unknown_reason.as_deref(), Some(reason));
        }
    }

    #[test]
    fn absent_cache_usage_does_not_require_cache_rates() {
        let pricing = ModelPricing::new(2.0, 10.0);
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 5,
            ..Default::default()
        };

        assert_eq!(calculate_charge_micro_usd(&usage, &pricing), Some(250));
    }

    #[test]
    fn invalid_provider_bucket_relationship_keeps_charge_unknown() {
        let pricing = ModelPricing::cache_aware(Some(2.0), Some(0.2), Some(2.5), Some(10.0));
        let usage = Usage {
            prompt_tokens: 10,
            cache_read_tokens: 8,
            cache_write_tokens: 4,
            ..Default::default()
        };

        let evidence = calculate_charge_evidence(&usage, &pricing, PricingSource::Configured);

        assert_eq!(evidence.status, ChargeStatus::Unknown);
        assert_eq!(evidence.charge_micro_usd, None);
        assert_eq!(
            evidence.unknown_reason.as_deref(),
            Some("invalid_input_buckets")
        );
    }

    #[test]
    fn context_tier_inherits_omitted_cache_rates_from_base() {
        let pricing = ModelPricing {
            input_micro_usd_per_token: Some(2.0),
            cache_read_micro_usd_per_token: Some(0.2),
            cache_write_micro_usd_per_token: Some(2.5),
            output_micro_usd_per_token: Some(10.0),
            context_tiers: vec![ContextTier {
                above_input_tokens: 50,
                input_micro_usd_per_token: Some(4.0),
                cache_read_micro_usd_per_token: None,
                cache_write_micro_usd_per_token: None,
                output_micro_usd_per_token: Some(20.0),
            }],
        };

        let resolved = pricing.resolve_for_input_tokens(100);
        assert_eq!(resolved.input_micro_usd_per_token, Some(4.0));
        assert_eq!(resolved.cache_read_micro_usd_per_token, Some(0.2));
        assert_eq!(resolved.cache_write_micro_usd_per_token, Some(2.5));
        assert_eq!(resolved.output_micro_usd_per_token, Some(20.0));
    }

    /// v0 bitrouter#463-A regression: partial pricing (input rate set,
    /// output rate missing) must NOT silently bill the output bucket as $0.
    #[test]
    fn partial_pricing_with_nonzero_output_returns_none() {
        let pricing = ModelPricing::partial(Some(2.0), None);
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 50, // nonzero, but rate is missing
            ..Default::default()
        };
        assert_eq!(
            calculate_charge_micro_usd(&usage, &pricing),
            None,
            "missing rate × nonzero usage MUST refuse to bill"
        );
    }

    /// Buckets at zero are charged zero even when the matching rate is
    /// missing — there's no work to bill.
    #[test]
    fn zero_usage_bucket_with_missing_rate_is_zero() {
        let pricing = ModelPricing::partial(Some(2.0), None);
        let usage = Usage {
            prompt_tokens: 100,
            completion_tokens: 0,
            ..Default::default()
        };
        assert_eq!(calculate_charge_micro_usd(&usage, &pricing), Some(200));
    }

    /// v0 audit B9: adversarial upstream usage gets clamped at MAX_TRUSTED_TOKENS.
    #[test]
    fn adversarial_token_count_is_clamped() {
        let pricing = ModelPricing::new(1.0, 1.0);
        let usage = Usage {
            prompt_tokens: u64::MAX,
            completion_tokens: 0,
            ..Default::default()
        };
        // u64::MAX would overflow the i64 cast; the clamp keeps the charge
        // at the bounded maximum.
        let charge = calculate_charge_micro_usd(&usage, &pricing).unwrap();
        assert_eq!(charge, MAX_TRUSTED_TOKENS as i64);
    }

    #[test]
    fn is_complete_predicate() {
        assert!(!ModelPricing::default().is_complete());
        assert!(!ModelPricing::partial(Some(1.0), None).is_complete());
        assert!(!ModelPricing::partial(None, Some(1.0)).is_complete());
        assert!(!ModelPricing::new(1.0, 1.0).is_complete());
        assert!(
            ModelPricing::cache_aware(Some(1.0), Some(0.1), Some(1.25), Some(1.0)).is_complete()
        );
    }

    #[test]
    fn table_resolve_keys_by_provider_and_service_id() {
        let mut table = PricingTable::new();
        // #443: service id differs from the public model name — still resolves.
        table.insert("openai", "gpt-5-2026-01", ModelPricing::new(1.0, 4.0));
        assert!(table.resolve("openai", "gpt-5-2026-01").is_some());
        assert!(table.resolve("openai", "gpt-5").is_none());
        assert!(table.resolve("anthropic", "gpt-5-2026-01").is_none());
    }

    /// Base ≤128k = 1.3/7.8 µ$/token; higher bracket >128k = 2.0/12.0.
    fn tiered() -> ModelPricing {
        ModelPricing {
            input_micro_usd_per_token: Some(1.3),
            cache_read_micro_usd_per_token: None,
            cache_write_micro_usd_per_token: None,
            output_micro_usd_per_token: Some(7.8),
            context_tiers: vec![ContextTier {
                above_input_tokens: 128_000,
                input_micro_usd_per_token: Some(2.0),
                cache_read_micro_usd_per_token: None,
                cache_write_micro_usd_per_token: None,
                output_micro_usd_per_token: Some(12.0),
            }],
        }
    }

    #[test]
    fn resolve_picks_base_at_or_below_threshold() {
        let p = tiered();
        assert_eq!(
            p.resolve_for_input_tokens(0).input_micro_usd_per_token,
            Some(1.3)
        );
        assert_eq!(
            p.resolve_for_input_tokens(128_000)
                .input_micro_usd_per_token,
            Some(1.3)
        );
    }

    #[test]
    fn resolve_picks_tier_above_threshold() {
        let p = tiered();
        let hi = p.resolve_for_input_tokens(128_001);
        assert_eq!(hi.input_micro_usd_per_token, Some(2.0));
        assert_eq!(hi.output_micro_usd_per_token, Some(12.0));
        assert!(hi.context_tiers.is_empty(), "resolved bracket is flat");
    }

    #[test]
    fn resolve_highest_applicable_tier_is_order_independent() {
        let p = ModelPricing {
            input_micro_usd_per_token: Some(1.0),
            cache_read_micro_usd_per_token: None,
            cache_write_micro_usd_per_token: None,
            output_micro_usd_per_token: Some(2.0),
            context_tiers: vec![
                ContextTier {
                    above_input_tokens: 256_000,
                    input_micro_usd_per_token: Some(4.0),
                    cache_read_micro_usd_per_token: None,
                    cache_write_micro_usd_per_token: None,
                    output_micro_usd_per_token: Some(8.0),
                },
                ContextTier {
                    above_input_tokens: 128_000,
                    input_micro_usd_per_token: Some(2.0),
                    cache_read_micro_usd_per_token: None,
                    cache_write_micro_usd_per_token: None,
                    output_micro_usd_per_token: Some(4.0),
                },
            ],
        };
        assert_eq!(
            p.resolve_for_input_tokens(1_000).input_micro_usd_per_token,
            Some(1.0)
        );
        assert_eq!(
            p.resolve_for_input_tokens(200_000)
                .input_micro_usd_per_token,
            Some(2.0)
        );
        assert_eq!(
            p.resolve_for_input_tokens(300_000)
                .input_micro_usd_per_token,
            Some(4.0)
        );
    }

    #[test]
    fn charge_uses_the_bracket_selected_by_prompt_size() {
        let pricing = tiered();
        // ≤128k → base: 100_000*1.3 + 1_000*7.8 = 137_800.
        let lo = Usage {
            prompt_tokens: 100_000,
            completion_tokens: 1_000,
            ..Default::default()
        };
        assert_eq!(calculate_charge_micro_usd(&lo, &pricing), Some(137_800));
        // >128k → tier: 200_000*2.0 + 1_000*12.0 = 412_000.
        let hi = Usage {
            prompt_tokens: 200_000,
            completion_tokens: 1_000,
            ..Default::default()
        };
        assert_eq!(calculate_charge_micro_usd(&hi, &pricing), Some(412_000));
    }

    #[test]
    fn flat_pricing_charges_identically_at_every_size() {
        // No tiers ⇒ base rate for any prompt size (back-compat).
        let pricing = ModelPricing::new(2.0, 10.0);
        for n in [10_u64, 200_000] {
            let usage = Usage {
                prompt_tokens: n,
                completion_tokens: 0,
                ..Default::default()
            };
            assert_eq!(
                calculate_charge_micro_usd(&usage, &pricing),
                Some((n as f64 * 2.0) as i64)
            );
        }
    }

    #[test]
    fn tier_inherits_base_rate_when_bucket_rate_is_omitted() {
        let pricing = ModelPricing {
            input_micro_usd_per_token: Some(1.3),
            cache_read_micro_usd_per_token: None,
            cache_write_micro_usd_per_token: None,
            output_micro_usd_per_token: Some(7.8),
            context_tiers: vec![ContextTier {
                above_input_tokens: 128_000,
                input_micro_usd_per_token: Some(2.0),
                cache_read_micro_usd_per_token: None,
                cache_write_micro_usd_per_token: None,
                output_micro_usd_per_token: None,
            }],
        };
        let hi = Usage {
            prompt_tokens: 200_000,
            completion_tokens: 1_000,
            ..Default::default()
        };
        assert_eq!(calculate_charge_micro_usd(&hi, &pricing), Some(407_800));
    }
}
