//! OpenCode Zen provider-specific request configuration.
//!
//! The Zen API serves models across multiple protocols depending on model
//! family: Claude models use the Anthropic Messages API, Gemini models use
//! the Google Generative AI API, GPT models use the OpenAI Responses API,
//! and all others use the OpenAI Chat Completions API.
//!
//! This module provides the model ID → protocol dispatch logic and
//! base URL derivation, following the same pattern as the Copilot module.

use std::collections::HashSet;
use std::sync::LazyLock;

/// The built-in provider name used for OpenCode Zen.
pub const ZEN_PROVIDER: &str = "opencode-zen";

/// Model IDs that use the Anthropic Messages API on Zen.
///
/// Derived from the `provider.npm == "@ai-sdk/anthropic"` field in models.dev.
/// Some models (big-pickle, free-tier minimax, qwen) use the Anthropic
/// protocol despite not being Claude models.
static ZEN_ANTHROPIC_MODELS: LazyLock<HashSet<&'static str>> = LazyLock::new(|| {
    let mut s = HashSet::new();
    s.insert("big-pickle");
    s.insert("minimax-m2.1-free");
    s.insert("minimax-m2.5-free");
    s.insert("qwen3.5-plus");
    s.insert("qwen3.6-plus");
    s
});

/// Returns `true` when a Zen model ID should use the Anthropic Messages API.
pub fn is_anthropic_model(model_id: &str) -> bool {
    model_id.starts_with("claude-")
        || model_id.starts_with("claude_")
        || ZEN_ANTHROPIC_MODELS.contains(model_id)
}

/// Returns `true` when a Zen model ID should use the Google Generative AI API.
pub fn is_google_model(model_id: &str) -> bool {
    model_id.starts_with("gemini-")
}

/// Returns `true` when a Zen model ID should use the OpenAI Responses API.
///
/// All GPT family models on Zen use the Responses endpoint.
pub fn is_responses_model(model_id: &str) -> bool {
    model_id.starts_with("gpt-5") || model_id == "gpt-5"
}

/// Derives the Anthropic-compatible API base from the Zen provider's base URL.
///
/// The Anthropic adapter appends `/v1/messages` itself, so we strip any
/// trailing `/v1` from the Zen base URL to avoid double-pathing.
pub fn anthropic_api_base(provider_api_base: Option<&str>) -> String {
    let base = provider_api_base
        .unwrap_or("https://opencode.ai/zen/v1")
        .trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base).to_owned()
}

/// Derives the Google-compatible API base from the Zen provider's base URL.
///
/// The Google adapter appends `/{api_version}/models/{model}:action` itself,
/// so we strip any trailing `/v1` to get the scheme+host root.
pub fn google_api_base(provider_api_base: Option<&str>) -> String {
    let base = provider_api_base
        .unwrap_or("https://opencode.ai/zen/v1")
        .trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claude_models_detected_as_anthropic() {
        assert!(is_anthropic_model("claude-sonnet-4-6"));
        assert!(is_anthropic_model("claude-haiku-4-5"));
        assert!(is_anthropic_model("claude-opus-4-7"));
        assert!(is_anthropic_model("claude-3-5-haiku"));
    }

    #[test]
    fn non_claude_anthropic_models_detected() {
        assert!(is_anthropic_model("big-pickle"));
        assert!(is_anthropic_model("qwen3.5-plus"));
        assert!(is_anthropic_model("qwen3.6-plus"));
        assert!(is_anthropic_model("minimax-m2.5-free"));
        assert!(is_anthropic_model("minimax-m2.1-free"));
    }

    #[test]
    fn openai_models_not_anthropic() {
        assert!(!is_anthropic_model("gpt-5.5"));
        assert!(!is_anthropic_model("glm-5.1"));
        assert!(!is_anthropic_model("minimax-m2.7"));
    }

    #[test]
    fn gemini_models_detected_as_google() {
        assert!(is_google_model("gemini-3-flash"));
        assert!(is_google_model("gemini-3.1-pro"));
        assert!(!is_google_model("gpt-5.4"));
    }

    #[test]
    fn gpt_models_detected_as_responses() {
        assert!(is_responses_model("gpt-5.5"));
        assert!(is_responses_model("gpt-5.4-pro"));
        assert!(is_responses_model("gpt-5.4-mini"));
        assert!(is_responses_model("gpt-5-nano"));
        assert!(is_responses_model("gpt-5"));
        assert!(is_responses_model("gpt-5.1-codex"));
        assert!(is_responses_model("gpt-5.3-codex-spark"));
        assert!(!is_responses_model("claude-sonnet-4-6"));
        assert!(!is_responses_model("glm-5.1"));
    }

    #[test]
    fn anthropic_base_url_strips_v1() {
        assert_eq!(
            anthropic_api_base(Some("https://opencode.ai/zen/v1")),
            "https://opencode.ai/zen"
        );
        assert_eq!(
            anthropic_api_base(Some("https://opencode.ai/zen/v1/")),
            "https://opencode.ai/zen"
        );
    }

    #[test]
    fn google_base_url_strips_v1() {
        assert_eq!(
            google_api_base(Some("https://opencode.ai/zen/v1")),
            "https://opencode.ai/zen"
        );
    }
}
