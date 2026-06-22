//! Parsed shape of the bitrouter provider-registry distribution artifacts.
//!
//! Source of truth: the public registry <https://github.com/bitrouter/provider-registry>.
//! It publishes two deterministic JSON files under `dist/`, each an envelope
//! `{ "data": [ … ] }`:
//!
//! - `providers.json` — the provider view: one entry per provider, each model
//!   carrying its dist-resolved `api_protocol` + `rate_limits` (the source
//!   YAML's glob patterns are expanded by the registry build, so bitrouter
//!   reads concrete values and runs no glob engine).
//! - `models.json` — the model view: one entry per canonical model. bitrouter
//!   consumes only `data[].id` (the authoritative canonical vocabulary, used to
//!   give the hosted gateway every model); the per-model `providers[]` reverse
//!   index is for other consumers.
//!
//! The structs below model only the fields bitrouter consumes; unknown fields
//! are ignored (no `deny_unknown_fields`) so the registry can add fields
//! without breaking this consumer.

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
    /// Every canonical model id from `models.json`.
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

/// One provider entry from `providers.json` (the provider view). The dist is
/// fully resolved: the provider-level glob `api_protocol` / `rate_limits` of the
/// source YAML are expanded onto each model, so no pattern fields appear here.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryProvider {
    /// Provider id (equals the registry filename stem and the `name` field).
    pub name: String,
    /// The provider's public upstream base URL (HTTPS).
    pub api_base: String,
    /// Declared model entries (canonical id + upstream id + resolved config).
    #[serde(default)]
    pub models: Vec<RegistryModel>,
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
}

/// One model a provider serves — a canonical id mapped to the provider's own
/// upstream id, with the dist-resolved per-(provider, model) config.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryModel {
    /// Canonical model id (`<org>/<model>`), the routing match key.
    pub id: String,
    /// The provider's own upstream model id (what is sent on the wire).
    pub provider_model_id: String,
    /// Resolved wire protocol for this (provider, model) pair — the dist
    /// already expanded the provider's glob patterns, so this is concrete.
    pub api_protocol: RegistryProtocol,
    /// Per-model pricing.
    #[serde(default)]
    pub pricing: Option<RegistryPricing>,
    /// Resolved rate limits for this (provider, model) pair, if any.
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

/// One entry from `models.json` (the model view). bitrouter consumes only the
/// `id` (the authoritative model vocabulary); the descriptive metadata and the
/// `providers[]` reverse index are ignored here.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct CanonicalModel {
    /// Canonical model id (`<org>/<model>`).
    pub id: String,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed real-shape sample of the resolved `dist/providers.json`: no
    /// provider-level glob arrays; each model carries a concrete `api_protocol`.
    const PROVIDERS_FIXTURE: &str = r#"{
        "data": [
            {
                "id": "anthropic",
                "name": "anthropic",
                "api_base": "https://api.anthropic.com/v1",
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
                        "api_protocol": "anthropic",
                        "capabilities": ["reasoning", "tools"],
                        "rate_limits": { "requests_per_minute": 60 },
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

    /// A trimmed sample of the model-view `dist/models.json`; bitrouter reads
    /// only `id`, ignoring metadata + the `providers[]` reverse index.
    const MODELS_FIXTURE: &str = r#"{
        "data": [
            { "id": "anthropic/claude-sonnet-4.6", "name": "Anthropic: Claude Sonnet 4.6",
              "max_input_tokens": 1000000,
              "providers": [ { "provider": "anthropic", "provider_model_id": "claude-sonnet-4-6", "api_protocol": "anthropic" } ] },
            { "id": "deepseek/deepseek-v3.2", "open_weights": true, "providers": [] }
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
        let m = &anthropic.models[0];
        assert_eq!(m.id, "anthropic/claude-sonnet-4.6");
        assert_eq!(m.provider_model_id, "claude-sonnet-4-6");
        // Resolved per-model protocol + rate limits (no glob to resolve here).
        assert_eq!(m.api_protocol, RegistryProtocol::Anthropic);
        assert_eq!(
            m.rate_limits.as_ref().and_then(|r| r.requests_per_minute),
            Some(60)
        );
        let pricing = m.pricing.as_ref().unwrap();
        assert_eq!(pricing.input_tokens.as_ref().unwrap().no_cache, Some(3.0));
        assert_eq!(pricing.output_tokens.as_ref().unwrap().text, Some(15.0));

        // The coding-plan defaults to subscription billing.
        assert_eq!(env.data[1].billing, Billing::Subscription);
        // The pool provider is byok=false → it will be filtered out at merge.
        assert!(!env.data[2].byok);
    }

    #[test]
    fn parses_models_envelope() {
        let env: Envelope<CanonicalModel> = serde_json::from_str(MODELS_FIXTURE).unwrap();
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
