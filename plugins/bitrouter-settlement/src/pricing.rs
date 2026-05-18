//! Model pricing and charge calculation.
//!
//! v0 bug defences baked in here:
//! - **#180** — a default (all-zero) `ModelPricing` is treated as *unconfigured*,
//!   not as *free*: the charge strategies WARN, emit `PricingUnavailable`, and
//!   skip the charge rather than silently settling zero.
//! - **#440 → #441** — pricing lookup covers every level; a `(provider, model)`
//!   miss is reported, not papered over.
//! - **#443 → #445** — the lookup is keyed by `(provider, service_id)` so a
//!   service id that differs from the public model name still resolves.

use std::collections::HashMap;

use bitrouter_sdk::language_model::Usage;

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
///   *unconfigured*: charge strategies WARN, emit `PricingUnavailable`, and
///   skip the charge rather than silently settling zero.
/// - **bitrouter#463-A** — if any token bucket with nonzero usage lacks a
///   rate, `calculate_charge_micro_usd` returns `None` and the charge is
///   skipped. A model with input rate set but `output_micro_usd_per_token`
///   missing no longer bills every output token at $0.
/// - **#440 → #441** — pricing lookup covers every level; a `(provider,
///   model)` miss is reported, not papered over.
/// - **#443 → #445** — the lookup is keyed by `(provider, service_id)` so a
///   service id that differs from the public model name still resolves.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelPricing {
    /// Micro-USD charged per prompt (input) token. `None` = unconfigured.
    pub input_micro_usd_per_token: Option<f64>,
    /// Micro-USD charged per completion (output) token. `None` = unconfigured.
    pub output_micro_usd_per_token: Option<f64>,
}

impl ModelPricing {
    /// Build a pricing entry with both rates set. Use [`partial`](Self::partial)
    /// when only some rates are known.
    pub fn new(input_micro_usd_per_token: f64, output_micro_usd_per_token: f64) -> Self {
        Self {
            input_micro_usd_per_token: Some(input_micro_usd_per_token),
            output_micro_usd_per_token: Some(output_micro_usd_per_token),
        }
    }

    /// Build a partial pricing entry. Buckets with nonzero usage but no rate
    /// are billed as `None` (charge is skipped).
    pub fn partial(input: Option<f64>, output: Option<f64>) -> Self {
        Self {
            input_micro_usd_per_token: input,
            output_micro_usd_per_token: output,
        }
    }

    /// Whether this pricing has no rates set at all — i.e. **unconfigured**.
    /// Distinct from `partial` (some rates set, some not) and from
    /// explicitly-zero rates.
    pub fn is_unconfigured(&self) -> bool {
        self.input_micro_usd_per_token.is_none() && self.output_micro_usd_per_token.is_none()
    }

    /// Whether *every* rate this struct exposes is set. Useful for a
    /// recommender / "cheapest" picker that should refuse placeholder
    /// entries (v0 bitrouter#463 / cloud#251 audit B5).
    pub fn is_complete(&self) -> bool {
        self.input_micro_usd_per_token.is_some() && self.output_micro_usd_per_token.is_some()
    }
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
            .copied()
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
pub fn calculate_charge_micro_usd(usage: &Usage, pricing: &ModelPricing) -> Option<i64> {
    let input = bucket_charge(usage.prompt_tokens, pricing.input_micro_usd_per_token)?;
    let output = bucket_charge(usage.completion_tokens, pricing.output_micro_usd_per_token)?;
    Some((input + output).round().max(0.0) as i64)
}

/// One token bucket's contribution to the charge. `Some(0.0)` when the bucket
/// is zero (no work to bill, rate may legitimately be missing); `None` when
/// the bucket has nonzero usage but no rate.
fn bucket_charge(tokens: u64, rate: Option<f64>) -> Option<f64> {
    let tokens = tokens.min(MAX_TRUSTED_TOKENS);
    match (tokens, rate) {
        (0, _) => Some(0.0),
        (n, Some(r)) => Some(n as f64 * r),
        (_, None) => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        assert!(ModelPricing::new(1.0, 1.0).is_complete());
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
}
