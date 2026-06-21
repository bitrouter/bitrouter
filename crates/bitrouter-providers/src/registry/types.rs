//! Parsed shape of the bitrouter provider-registry distribution artifacts.
//!
//! Source of truth: the public registry <https://github.com/bitrouter/provider-registry>.
//! It publishes two deterministic JSON files under `dist/`, each an envelope
//! `{ "data": [ … ] }`:
//!
//! - `providers.json` — one entry per provider: which canonical models it
//!   serves, at what price, over which wire protocol. Mirrors the registry's
//!   `ProviderFile` Zod schema (`scripts/schema.ts`).
//! - `canonical.json` — the shared `<org>/<model>` model vocabulary. Mirrors
//!   the registry's `CanonicalModel` Zod schema.
//!
//! The structs below model only the fields bitrouter consumes; unknown fields
//! are ignored (no `deny_unknown_fields`) so the registry can add fields
//! without breaking this consumer.

use std::collections::BTreeMap;

use serde::Deserialize;

/// The distribution envelope shared by both dist files: `{ "data": [ … ] }`.
#[derive(Debug, Clone, Deserialize)]
pub struct Envelope<T> {
    /// The list of entries.
    pub data: Vec<T>,
}

/// The merged registry data bitrouter caches and consumes: the provider list
/// plus the canonical model vocabulary.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryData {
    /// Every provider entry from `providers.json`.
    pub providers: Vec<RegistryProvider>,
    /// Every canonical model from `canonical.json`.
    pub canonical: Vec<CanonicalModel>,
}

/// The wire protocol a provider serves, in the registry's vocabulary. Maps onto
/// bitrouter's [`ApiProtocol`](bitrouter_sdk::language_model::types::ApiProtocol)
/// at merge time: `openai`→Chat Completions, `anthropic`→Messages,
/// `google`→Generate Content, `responses`→Responses.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryProtocol {
    /// OpenAI Chat Completions.
    Openai,
    /// Anthropic Messages.
    Anthropic,
    /// Google Generate Content.
    Google,
    /// OpenAI Responses.
    Responses,
}

/// How a caller pays a provider — mirrors the registry `billing` field.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Billing {
    /// Pay-as-you-go, metered per token (the default).
    #[default]
    Token,
    /// Flat-rate plan (e.g. a first-party coding plan).
    Subscription,
}

/// A single pattern→value entry as serialized by the registry: a one-key map,
/// e.g. `{ "*": "openai" }` or `{ "claude-*": "anthropic" }`.
pub type PatternEntry<T> = BTreeMap<String, T>;

/// One provider entry from `providers.json`.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryProvider {
    /// Provider id (equals the registry filename stem and the `name` field).
    pub name: String,
    /// The provider's public upstream base URL (HTTPS).
    pub api_base: String,
    /// Wire protocol per model-id glob (pattern → protocol). A bare `"*"` entry
    /// is the provider-wide default.
    #[serde(default)]
    pub api_protocol: Vec<PatternEntry<RegistryProtocol>>,
    /// Declared model entries (canonical id + upstream id + pricing/caps).
    #[serde(default)]
    pub models: Vec<RegistryModel>,
    /// Rate limits per model-id glob.
    #[serde(default)]
    pub rate_limits: Vec<PatternEntry<RegistryRateLimits>>,
    /// `active` | `staging` | `suspended` | `withdrawn` — only `active` routes.
    pub status: String,
    /// `true` marks an unaffiliated community reseller; `false` (default) is a
    /// first-party / official upstream.
    #[serde(default)]
    pub community: bool,
    /// Whether callers may bring their own key. Only BYOK providers are merged.
    #[serde(default = "default_true")]
    pub byok: bool,
    /// How a caller pays this provider (`token` | `subscription`).
    #[serde(default)]
    pub billing: Billing,
}

fn default_true() -> bool {
    true
}

impl RegistryProvider {
    /// Whether this provider is routable (`status == "active"`).
    pub fn is_active(&self) -> bool {
        self.status == "active"
    }

    /// The provider-wide default protocol — the value of the `"*"` pattern
    /// entry, if any.
    pub fn default_protocol(&self) -> Option<RegistryProtocol> {
        self.api_protocol
            .iter()
            .find_map(|entry| entry.get("*").copied())
    }
}

/// One model a provider serves — a canonical id mapped to the provider's own
/// upstream id, with optional per-model overrides.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryModel {
    /// Canonical model id (`<org>/<model>`), the routing match key.
    pub id: String,
    /// The provider's own upstream model id (what is sent on the wire).
    pub provider_model_id: String,
    /// Per-model protocol override.
    #[serde(default)]
    pub api_protocol: Option<RegistryProtocol>,
    /// Per-model pricing.
    #[serde(default)]
    pub pricing: Option<RegistryPricing>,
    /// Per-model rate limits.
    #[serde(default)]
    pub rate_limits: Option<RegistryRateLimits>,
}

/// Per-token pricing for a registry model. bitrouter consumes the base
/// no-cache input rate, the text output rate, and any context tiers; other
/// rates (cache read/write, reasoning) are ignored by OSS metering.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryPricing {
    /// Input-token rates.
    #[serde(default)]
    pub input_tokens: Option<InputTokenPricing>,
    /// Output-token rates.
    #[serde(default)]
    pub output_tokens: Option<OutputTokenPricing>,
    /// Higher context-length pricing brackets (step function on input size).
    #[serde(default)]
    pub context_tiers: Vec<RegistryContextTier>,
}

/// Input-token rates (USD per 1M tokens == µUSD per token).
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct InputTokenPricing {
    /// Uncached input rate.
    #[serde(default)]
    pub no_cache: Option<f64>,
}

/// Output-token rates (USD per 1M tokens == µUSD per token).
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct OutputTokenPricing {
    /// Text completion rate.
    #[serde(default)]
    pub text: Option<f64>,
}

/// A higher context-pricing bracket: rates that apply once the prompt's total
/// input-token count strictly exceeds `above_input_tokens`.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryContextTier {
    /// Exclusive lower bound on total input tokens for this bracket.
    pub above_input_tokens: u64,
    /// Input-token rates for this bracket.
    #[serde(default)]
    pub input_tokens: Option<InputTokenPricing>,
    /// Output-token rates for this bracket.
    #[serde(default)]
    pub output_tokens: Option<OutputTokenPricing>,
}

/// Provider / model rate limits.
#[derive(Debug, Clone, Default, serde::Serialize, Deserialize)]
pub struct RegistryRateLimits {
    /// Requests per minute.
    #[serde(default)]
    pub requests_per_minute: Option<u32>,
    /// Tokens per minute.
    #[serde(default)]
    pub tokens_per_minute: Option<u32>,
}

/// One canonical model from `canonical.json`. bitrouter consumes the id (the
/// authoritative model vocabulary); other descriptive fields are ignored.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct CanonicalModel {
    /// Canonical model id (`<org>/<model>`).
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed real-shape sample of `dist/providers.json`.
    const PROVIDERS_FIXTURE: &str = r#"{
        "data": [
            {
                "id": "anthropic",
                "name": "anthropic",
                "api_base": "https://api.anthropic.com/v1",
                "api_protocol": [ { "*": "anthropic" } ],
                "auth_scheme": "x-api-key",
                "billing": "token",
                "byok": true,
                "community": false,
                "status": "active",
                "weight": 1,
                "models": [
                    {
                        "id": "anthropic/claude-sonnet-4.6",
                        "provider_model_id": "claude-sonnet-4-6",
                        "capabilities": ["reasoning", "tools"],
                        "pricing": {
                            "input_tokens": { "no_cache": 3, "cache_read": 0.3 },
                            "output_tokens": { "text": 15 }
                        }
                    }
                ]
            },
            {
                "id": "zai-coding-plan",
                "name": "zai-coding-plan",
                "api_base": "https://api.z.ai/api/coding/paas/v4",
                "api_protocol": [ { "*": "openai" } ],
                "billing": "subscription",
                "status": "active",
                "models": []
            },
            {
                "id": "bitrouter",
                "name": "bitrouter",
                "api_base": "https://provider-api.bitrouter.ai/v1",
                "byok": false,
                "status": "active",
                "models": []
            }
        ]
    }"#;

    const CANONICAL_FIXTURE: &str = r#"{
        "data": [
            { "id": "anthropic/claude-sonnet-4.6", "name": "Anthropic: Claude Sonnet 4.6", "max_input_tokens": 1000000 },
            { "id": "deepseek/deepseek-v3.2", "open_weights": true }
        ]
    }"#;

    #[test]
    fn parses_providers_envelope() {
        let env: Envelope<RegistryProvider> = serde_json::from_str(PROVIDERS_FIXTURE).unwrap();
        assert_eq!(env.data.len(), 3);
        let anthropic = &env.data[0];
        assert_eq!(anthropic.name, "anthropic");
        assert!(anthropic.is_active());
        assert!(anthropic.byok);
        assert!(!anthropic.community);
        assert_eq!(anthropic.billing, Billing::Token);
        assert_eq!(
            anthropic.default_protocol(),
            Some(RegistryProtocol::Anthropic)
        );
        let m = &anthropic.models[0];
        assert_eq!(m.id, "anthropic/claude-sonnet-4.6");
        assert_eq!(m.provider_model_id, "claude-sonnet-4-6");
        let pricing = m.pricing.as_ref().unwrap();
        assert_eq!(pricing.input_tokens.as_ref().unwrap().no_cache, Some(3.0));
        assert_eq!(pricing.output_tokens.as_ref().unwrap().text, Some(15.0));

        // The coding-plan defaults to subscription billing.
        assert_eq!(env.data[1].billing, Billing::Subscription);
        // The pool provider is byok=false → it will be filtered out at merge.
        assert!(!env.data[2].byok);
    }

    #[test]
    fn parses_canonical_envelope() {
        let env: Envelope<CanonicalModel> = serde_json::from_str(CANONICAL_FIXTURE).unwrap();
        assert_eq!(env.data.len(), 2);
        assert_eq!(env.data[0].id, "anthropic/claude-sonnet-4.6");
        assert_eq!(env.data[1].id, "deepseek/deepseek-v3.2");
    }

    #[test]
    fn unknown_fields_are_ignored() {
        // The registry may add fields OSS doesn't model — they must not break parsing.
        let src = r#"{ "data": [ {
            "name": "x", "api_base": "https://x.test/v1", "status": "active",
            "models": [], "some_future_field": { "nested": true }
        } ] }"#;
        let env: Envelope<RegistryProvider> = serde_json::from_str(src).unwrap();
        assert_eq!(env.data[0].name, "x");
    }
}
