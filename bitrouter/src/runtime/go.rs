//! OpenCode Go provider-specific request configuration.
//!
//! The Go API serves models across multiple protocols depending on model
//! family. Most models use the OpenAI Chat Completions API, but some
//! (MiniMax M2.7, Qwen3.5/3.6 Plus) use the Anthropic Messages API.
//!
//! This module provides the model ID → protocol dispatch logic and
//! base URL derivation, following the same pattern as the Copilot module.

/// The built-in provider name used for OpenCode Go.
pub const GO_PROVIDER: &str = "opencode-go";

/// Returns `true` when a Go model ID should use the Anthropic Messages API.
///
/// Derived from the `provider.npm == "@ai-sdk/anthropic"` field in models.dev.
pub fn is_anthropic_model(model_id: &str) -> bool {
    matches!(model_id, "minimax-m2.7" | "qwen3.5-plus" | "qwen3.6-plus")
}

/// Derives the Anthropic-compatible API base from the Go provider's base URL.
///
/// The Anthropic adapter appends `/v1/messages` itself, so we strip any
/// trailing `/v1` from the Go base URL to avoid double-pathing.
pub fn anthropic_api_base(provider_api_base: Option<&str>) -> String {
    let base = provider_api_base
        .unwrap_or("https://opencode.ai/zen/go/v1")
        .trim_end_matches('/');
    base.strip_suffix("/v1").unwrap_or(base).to_owned()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn go_anthropic_models_detected() {
        assert!(is_anthropic_model("minimax-m2.7"));
        assert!(is_anthropic_model("qwen3.5-plus"));
        assert!(is_anthropic_model("qwen3.6-plus"));
    }

    #[test]
    fn go_non_anthropic_models() {
        assert!(!is_anthropic_model("glm-5.1"));
        assert!(!is_anthropic_model("kimi-k2.6"));
        assert!(!is_anthropic_model("deepseek-v4-pro"));
        assert!(!is_anthropic_model("mimo-v2-pro"));
        assert!(!is_anthropic_model("minimax-m2.5"));
    }

    #[test]
    fn anthropic_base_url_strips_v1() {
        assert_eq!(
            anthropic_api_base(Some("https://opencode.ai/zen/go/v1")),
            "https://opencode.ai/zen/go"
        );
        assert_eq!(
            anthropic_api_base(Some("https://opencode.ai/zen/go/v1/")),
            "https://opencode.ai/zen/go"
        );
    }
}
