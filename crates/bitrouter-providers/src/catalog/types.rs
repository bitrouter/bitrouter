//! Parsed shape of `https://models.dev/api.json`.
//!
//! Schema reference: <https://models.dev/api> — the live JSON is the
//! authority; the types below model the fields v1 reads. Fields not used by
//! bitrouter (currently `npm`, `last_updated`, `release_date`, `knowledge`)
//! are kept on the struct so that round-tripping doesn't silently drop them.
//!
//! Pricing convention: every `cost.*` field is **USD per 1 million tokens**
//! (verified by spot-checking Claude Opus 4.1: `cost.input = 15` matches
//! Anthropic's published "$15 / M input tokens" rate). Conversion to v1's
//! `input_micro_usd_per_token` is the identity — 15 USD / 1e6 tokens =
//! 15 µUSD / token.

use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

/// Top-level: `{provider_id: ProviderCatalogEntry}`.
pub type Catalog = BTreeMap<String, ProviderCatalogEntry>;

/// One provider's catalog entry — provider-level metadata + a map of models.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ProviderCatalogEntry {
    /// Provider id (matches the top-level map key).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Env var names the provider expects for credentials. Bitrouter does not
    /// rely on this — auth is declared in the compiled-in [`crate::ProviderEntry`]
    /// — but we keep it for diff / drift detection.
    #[serde(default)]
    pub env: Vec<String>,
    /// Default API base URL. `null` for providers that don't expose one in
    /// the catalog (e.g. Anthropic — operators always use the SDK's default).
    #[serde(default)]
    pub api: Option<String>,
    /// npm package name for the AI SDK adapter.
    #[serde(default)]
    pub npm: Option<String>,
    /// Documentation link.
    #[serde(default)]
    pub doc: Option<String>,
    /// Map of model id → metadata.
    #[serde(default)]
    pub models: BTreeMap<String, ModelMetadata>,
}

/// One model's published metadata.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ModelMetadata {
    /// Model id (matches the per-provider map key).
    pub id: String,
    /// Display name.
    pub name: String,
    /// Model family (`claude-opus`, `gpt`, `gemini-2-5`, …). Optional.
    #[serde(default)]
    pub family: Option<String>,
    /// Whether the model can take file attachments.
    #[serde(default)]
    pub attachment: bool,
    /// Whether the model emits extended reasoning content.
    #[serde(default)]
    pub reasoning: bool,
    /// Whether the model supports tool / function calling.
    #[serde(default)]
    pub tool_call: bool,
    /// Whether the model supports structured-output JSON-schema constraints.
    #[serde(default)]
    pub structured_output: bool,
    /// Whether the `temperature` parameter is honoured (some reasoning
    /// models fix it to 1.0).
    #[serde(default)]
    pub temperature: bool,
    /// Knowledge cutoff (YYYY-MM or YYYY-MM-DD).
    #[serde(default)]
    pub knowledge: Option<String>,
    /// Release date (YYYY-MM-DD).
    #[serde(default)]
    pub release_date: Option<String>,
    /// Last-updated date (YYYY-MM-DD).
    #[serde(default)]
    pub last_updated: Option<String>,
    /// Input / output modality lists.
    #[serde(default)]
    pub modalities: Modalities,
    /// Whether this is an open-weights model.
    #[serde(default)]
    pub open_weights: bool,
    /// Context, input, output token limits.
    #[serde(default)]
    pub limit: ModelLimit,
    /// Per-token cost (USD per 1M tokens).
    #[serde(default)]
    pub cost: ModelCost,
}

/// Input / output modalities.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Modalities {
    /// Input modality tags (`"text"`, `"image"`, `"pdf"`, `"audio"`, `"video"`).
    #[serde(default)]
    pub input: Vec<String>,
    /// Output modality tags.
    #[serde(default)]
    pub output: Vec<String>,
}

/// Token-count limits. Some providers split `context` (total) from `input`
/// and `output` caps (e.g. GPT-5: 400k context but 272k input, 128k output).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelLimit {
    /// Total context window (tokens).
    #[serde(default)]
    pub context: Option<u64>,
    /// Maximum input tokens (when the provider distinguishes from `context`).
    #[serde(default)]
    pub input: Option<u64>,
    /// Maximum output tokens.
    #[serde(default)]
    pub output: Option<u64>,
}

/// Per-token cost in USD per 1 million tokens. Identical to micro-USD per
/// token: bitrouter's settlement pricing stores rates in µUSD/token, so any
/// `cost.input` value here drops in directly.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelCost {
    /// Per-1M input tokens (µUSD per token).
    #[serde(default)]
    pub input: Option<f64>,
    /// Per-1M output tokens (µUSD per token).
    #[serde(default)]
    pub output: Option<f64>,
    /// Per-1M cached-read tokens (µUSD per token).
    #[serde(default)]
    pub cache_read: Option<f64>,
    /// Per-1M cache-write tokens (µUSD per token).
    #[serde(default)]
    pub cache_write: Option<f64>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A trimmed real-shape sample. Sourced from a `curl` against
    /// `https://models.dev/api.json` on 2026-05-19.
    const FIXTURE: &str = r#"{
        "anthropic": {
            "id": "anthropic",
            "name": "Anthropic",
            "env": ["ANTHROPIC_API_KEY"],
            "api": null,
            "npm": "@ai-sdk/anthropic",
            "doc": "https://docs.anthropic.com/en/docs/about-claude/models",
            "models": {
                "claude-opus-4-1-20250805": {
                    "id": "claude-opus-4-1-20250805",
                    "name": "Claude Opus 4.1",
                    "family": "claude-opus",
                    "attachment": true,
                    "reasoning": true,
                    "tool_call": true,
                    "temperature": true,
                    "knowledge": "2025-03-31",
                    "release_date": "2025-08-05",
                    "last_updated": "2025-08-05",
                    "modalities": {"input": ["text", "image", "pdf"], "output": ["text"]},
                    "open_weights": false,
                    "limit": {"context": 200000, "output": 32000},
                    "cost": {"input": 15, "output": 75, "cache_read": 1.5, "cache_write": 18.75}
                }
            }
        },
        "openai": {
            "id": "openai",
            "name": "OpenAI",
            "env": ["OPENAI_API_KEY"],
            "api": "https://api.openai.com/v1",
            "doc": "https://platform.openai.com/docs/api-reference",
            "models": {
                "gpt-5": {
                    "id": "gpt-5",
                    "name": "GPT-5",
                    "family": "gpt",
                    "attachment": true,
                    "reasoning": true,
                    "tool_call": true,
                    "structured_output": true,
                    "temperature": false,
                    "modalities": {"input": ["text", "image"], "output": ["text"]},
                    "limit": {"context": 400000, "input": 272000, "output": 128000},
                    "cost": {"input": 1.25, "output": 10, "cache_read": 0.125}
                }
            }
        }
    }"#;

    #[test]
    fn parses_real_shape() {
        let catalog: Catalog = serde_json::from_str(FIXTURE).unwrap();
        assert_eq!(catalog.len(), 2);

        let anthropic = catalog.get("anthropic").unwrap();
        assert!(anthropic.api.is_none(), "anthropic catalog `api` is null");
        let opus = anthropic.models.get("claude-opus-4-1-20250805").unwrap();
        assert_eq!(opus.cost.input, Some(15.0));
        assert_eq!(opus.cost.output, Some(75.0));
        assert_eq!(opus.cost.cache_read, Some(1.5));
        assert_eq!(opus.cost.cache_write, Some(18.75));
        assert_eq!(opus.limit.context, Some(200_000));
        assert!(opus.modalities.input.contains(&"pdf".to_string()));

        let openai = catalog.get("openai").unwrap();
        let gpt5 = openai.models.get("gpt-5").unwrap();
        // GPT-5 splits `context` vs `input` — the catalog reports both.
        assert_eq!(gpt5.limit.context, Some(400_000));
        assert_eq!(gpt5.limit.input, Some(272_000));
        assert_eq!(gpt5.limit.output, Some(128_000));
        assert!(!gpt5.temperature, "gpt-5 reasoning fixes temperature");
        // cache_write absent → None (must not coerce to 0).
        assert_eq!(gpt5.cost.cache_write, None);
    }

    #[test]
    fn missing_optional_fields_default_safely() {
        // Minimal valid entry — everything else defaults.
        let src = r#"{
            "stub": {
                "id": "stub",
                "name": "Stub",
                "models": {
                    "tiny": {"id": "tiny", "name": "Tiny"}
                }
            }
        }"#;
        let catalog: Catalog = serde_json::from_str(src).unwrap();
        let tiny = catalog["stub"].models.get("tiny").unwrap();
        assert_eq!(tiny.cost.input, None);
        assert_eq!(tiny.limit.context, None);
        assert!(tiny.modalities.input.is_empty());
    }
}
