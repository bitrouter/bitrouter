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

/// Per-model pricing, in **micro-USD per token**. `Default` is all-zero, which
/// is read as "unconfigured" — see #180.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct ModelPricing {
    /// Micro-USD charged per prompt (input) token.
    pub input_micro_usd_per_token: f64,
    /// Micro-USD charged per completion (output) token.
    pub output_micro_usd_per_token: f64,
}

impl ModelPricing {
    /// Build a pricing entry.
    pub fn new(input_micro_usd_per_token: f64, output_micro_usd_per_token: f64) -> Self {
        Self {
            input_micro_usd_per_token,
            output_micro_usd_per_token,
        }
    }

    /// Whether this pricing is the all-zero default — i.e. **unconfigured**.
    /// Distinct from "explicitly priced at zero", which is not representable
    /// here (and is the whole point of #180).
    pub fn is_unconfigured(&self) -> bool {
        *self == ModelPricing::default()
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

/// Compute the charge for a request, in micro-USD, from its usage and pricing.
/// Rounds to the nearest whole micro-USD; never negative.
pub fn calculate_charge_micro_usd(usage: &Usage, pricing: &ModelPricing) -> i64 {
    let input = usage.prompt_tokens as f64 * pricing.input_micro_usd_per_token;
    let output = usage.completion_tokens as f64 * pricing.output_micro_usd_per_token;
    (input + output).round().max(0.0) as i64
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
        assert_eq!(calculate_charge_micro_usd(&usage, &pricing), 700);
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
