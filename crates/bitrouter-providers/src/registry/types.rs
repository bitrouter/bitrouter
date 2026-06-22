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

use std::collections::BTreeMap;

use serde::Deserialize;

use bitrouter_sdk::language_model::types::{ApiProtocol, ProtocolList};

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
/// bitrouter's [`ApiProtocol`] at merge time: `openai`→Chat Completions,
/// `anthropic`→Messages, `google`→Generate Content, `responses`→Responses.
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

impl RegistryProtocol {
    /// Map onto bitrouter's wire-protocol enum.
    pub fn to_api_protocol(self) -> ApiProtocol {
        match self {
            RegistryProtocol::Openai => ApiProtocol::ChatCompletions,
            RegistryProtocol::Anthropic => ApiProtocol::Messages,
            RegistryProtocol::Google => ApiProtocol::GenerateContent,
            RegistryProtocol::Responses => ApiProtocol::Responses,
        }
    }
}

/// A wire-protocol value in the dist — a single protocol or an ordered set
/// (most-preferred first), e.g. `openai` or `[openai, responses]`.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(untagged)]
pub enum ProtocolSet {
    /// A single protocol.
    One(RegistryProtocol),
    /// An ordered set, most-preferred first.
    Many(Vec<RegistryProtocol>),
}

impl ProtocolSet {
    /// The protocols as an ordered slice-owning vec.
    pub fn to_vec(&self) -> Vec<RegistryProtocol> {
        match self {
            ProtocolSet::One(p) => vec![*p],
            ProtocolSet::Many(v) => v.clone(),
        }
    }

    /// Map onto bitrouter's ordered [`ProtocolList`] (most-preferred first).
    pub fn to_protocol_list(&self) -> ProtocolList {
        ProtocolList(
            self.to_vec()
                .into_iter()
                .map(RegistryProtocol::to_api_protocol)
                .collect(),
        )
    }
}

/// Registry provider classification — drives the routing-priority class.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryKind {
    /// Official / first-party upstream.
    FirstParty,
    /// Aggregator gateway fronting other makers' models.
    Gateway,
    /// The bitrouter hosted gateway.
    Cloud,
    /// Unaffiliated community reseller.
    ThirdParty,
}

/// Outbound credential scheme declared by the registry — see the registry's
/// `Auth`. Only public config (env/header/handler names + public params); never
/// a secret. Maps onto the compiled-in [`AuthScheme`](crate::AuthScheme).
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RegistryAuthKind {
    /// `Authorization: Bearer <env>`.
    Bearer,
    /// `<header>: <env>` plus any constant `extra_headers`.
    Header,
    /// OAuth flow resolved by a named handler in the consumer.
    Oauth,
    /// SDK-driven native auth resolved by a named handler in the consumer.
    Native,
}

/// The registry's structured auth declaration.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryAuth {
    /// The credential scheme.
    pub kind: RegistryAuthKind,
    /// Env var holding the credential (bearer/header).
    #[serde(default)]
    pub env: Option<String>,
    /// Header carrying the credential (header kind).
    #[serde(default)]
    pub header: Option<String>,
    /// Constant headers sent alongside the credential.
    #[serde(default)]
    pub extra_headers: Option<BTreeMap<String, String>>,
    /// Named handler in the consumer's registry (oauth/native).
    #[serde(default)]
    pub handler: Option<String>,
    /// Handler-specific public params (client_id, scopes, endpoints, …).
    #[serde(default)]
    pub params: Option<BTreeMap<String, serde_json::Value>>,
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

/// One provider entry from `providers.json` (the provider view). For a curated
/// provider the source-YAML glob `api_protocol` / `rate_limits` are resolved
/// onto each model (so the top-level globs are empty); for an `auto_discover`
/// provider the catalog is runtime-discovered, so the provider-level
/// `api_protocol` globs are kept here and applied to discovered models.
#[derive(Debug, Clone, serde::Serialize, Deserialize)]
pub struct RegistryProvider {
    /// Provider id (equals the registry filename stem and the `name` field).
    pub name: String,
    /// Human-readable display name (UI only), if declared.
    #[serde(default)]
    pub display_name: Option<String>,
    /// The provider's public upstream base URL (HTTPS).
    pub api_base: String,
    /// Provider-level wire-protocol globs (pattern → protocol set). Present only
    /// for `auto_discover` providers; empty for curated providers (resolved onto
    /// each model instead).
    #[serde(default)]
    pub api_protocol: Vec<BTreeMap<String, ProtocolSet>>,
    /// Per-protocol base-URL override, keyed by protocol name.
    #[serde(default)]
    pub protocol_endpoints: Option<BTreeMap<String, String>>,
    /// Declared model entries (canonical id + upstream id + resolved config).
    #[serde(default)]
    pub models: Vec<RegistryModel>,
    /// `active` | `staging` | `suspended` | `withdrawn` — only `active` routes.
    pub status: String,
    /// Transport+auth-only provider whose catalog is discovered at runtime.
    #[serde(default)]
    pub auto_discover: bool,
    /// Provider classification, if declared (drives the routing class). When
    /// absent the consumer derives it from `community`.
    #[serde(default)]
    pub kind: Option<RegistryKind>,
    /// Structured auth declaration, if the registry knows the scheme.
    #[serde(default)]
    pub auth: Option<RegistryAuth>,
    /// Link to the provider's official API documentation, if declared.
    #[serde(default)]
    pub doc_url: Option<String>,
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
    /// Resolved wire protocol(s) for this (provider, model) pair — the dist
    /// expanded the provider's glob patterns, so this is concrete (a single
    /// protocol or an ordered set, e.g. `[openai, responses]`).
    pub api_protocol: ProtocolSet,
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
        assert_eq!(
            m.api_protocol,
            ProtocolSet::One(RegistryProtocol::Anthropic)
        );
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
