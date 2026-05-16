use std::fmt;
use std::future::Future;
use std::sync::Arc;

use serde::{Deserialize, Serialize};

use crate::errors::Result;
use crate::models::language::call_options::ReasoningEffort;
use crate::routers::content::RouteContext;

// ── API protocol ──────────────────────────────────────────────────

/// The API protocol / wire format that an endpoint uses.
///
/// Model protocols determine how LLM requests are serialized (OpenAI chat
/// completions, Anthropic messages, Google generative AI). Tool protocols
/// determine how tool discovery and invocation work (MCP, REST).
///
/// A provider may default to one protocol but individual endpoints can
/// override it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiProtocol {
    // Model protocols
    Openai,
    Anthropic,
    Google,
    // Tool protocols
    Mcp,
    Rest,
    // Agent protocols
    Acp,
}

impl fmt::Display for ApiProtocol {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::Openai => "openai",
            Self::Anthropic => "anthropic",
            Self::Google => "google",
            Self::Mcp => "mcp",
            Self::Rest => "rest",
            Self::Acp => "acp",
        })
    }
}

// ── Routing ──────────────────────────────────────────────────────

/// Body-level overrides resolved from a preset attached to a routed request.
///
/// All fields are defaults — they take effect only when the request leaves
/// the corresponding field unset (OpenRouter-style shallow merge). Filter
/// handlers consume this value via a per-protocol applicator that knows how
/// each field maps onto the protocol's request struct.
#[derive(Debug, Clone, Default)]
pub struct AppliedPreset {
    /// System prompt to use when the request has no system message of its own.
    pub system: Option<String>,
    pub temperature: Option<f32>,
    pub top_p: Option<f32>,
    pub top_k: Option<u32>,
    pub max_tokens: Option<u32>,
    pub stop_sequences: Option<Vec<String>>,
    pub presence_penalty: Option<f32>,
    pub frequency_penalty: Option<f32>,
    pub reasoning_effort: Option<ReasoningEffort>,
}

impl AppliedPreset {
    /// Returns true when no field is populated — equivalent to "no preset".
    pub fn is_empty(&self) -> bool {
        self.system.is_none()
            && self.temperature.is_none()
            && self.top_p.is_none()
            && self.top_k.is_none()
            && self.max_tokens.is_none()
            && self.stop_sequences.is_none()
            && self.presence_penalty.is_none()
            && self.frequency_penalty.is_none()
            && self.reasoning_effort.is_none()
    }
}

/// The resolved target for a routed request (model or tool).
#[derive(Debug, Clone)]
pub struct RoutingTarget {
    /// The provider name to route to.
    pub provider_name: String,
    /// Upstream service identifier: model ID for language models, tool ID for tools.
    pub service_id: String,
    /// The resolved API protocol for this endpoint.
    pub api_protocol: ApiProtocol,
    /// Per-target API key override.
    ///
    /// When `Some`, downstream model/tool routers should prefer this credential
    /// over the provider's default `api_key`. Populated by per-endpoint
    /// configuration (see `Endpoint.api_key`) or by a [`TargetOverlay`] that
    /// runs after routing.
    pub api_key_override: Option<String>,
    /// Per-target API base URL override.
    ///
    /// When `Some`, downstream model/tool routers should prefer this base URL
    /// over the provider's default `api_base`. Populated by per-endpoint
    /// configuration (see `Endpoint.api_base`) or by a [`TargetOverlay`].
    pub api_base_override: Option<String>,
    /// Body-level overrides resolved from a `@preset` reference in the model
    /// string. When set, filter handlers shallow-merge these defaults onto
    /// the inbound request before dispatch.
    pub preset: Option<AppliedPreset>,
}

/// A single entry in the route listing, describing a configured route.
#[derive(Debug, Clone)]
pub struct RouteEntry {
    /// The virtual service name (e.g. "default", "gpt-4o", "create_issue").
    pub name: String,
    /// The provider name this route resolves to.
    pub provider: String,
    /// The API protocol the provider uses.
    pub protocol: ApiProtocol,
}

/// Input token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct InputTokenPricing {
    /// Cost per million non-cached input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub no_cache: Option<f64>,
    /// Cost per million cache-read input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_read: Option<f64>,
    /// Cost per million cache-write input tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cache_write: Option<f64>,
}

impl InputTokenPricing {
    fn is_empty(&self) -> bool {
        self.no_cache.is_none() && self.cache_read.is_none() && self.cache_write.is_none()
    }
}

/// Output token pricing per million tokens.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OutputTokenPricing {
    /// Cost per million text output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub text: Option<f64>,
    /// Cost per million reasoning output tokens.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reasoning: Option<f64>,
    /// Cost per million image output tokens. Reserved — not yet wired into
    /// `calculate_cost`; multimodal output billing will land alongside the
    /// matching usage bucket in `LanguageModelOutputTokens`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub image: Option<f64>,
    /// Cost per million audio output tokens. Reserved — see `image`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub audio: Option<f64>,
}

impl OutputTokenPricing {
    fn is_empty(&self) -> bool {
        self.text.is_none()
            && self.reasoning.is_none()
            && self.image.is_none()
            && self.audio.is_none()
    }
}

/// Token pricing per million tokens for a model.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct ModelPricing {
    #[serde(default, skip_serializing_if = "InputTokenPricing::is_empty")]
    pub input_tokens: InputTokenPricing,
    #[serde(default, skip_serializing_if = "OutputTokenPricing::is_empty")]
    pub output_tokens: OutputTokenPricing,
}

impl ModelPricing {
    /// Returns `true` when no pricing data is set.
    pub fn is_empty(&self) -> bool {
        self.input_tokens.is_empty() && self.output_tokens.is_empty()
    }

    /// Returns `true` when at minimum `input_tokens.no_cache` and
    /// `output_tokens.text` are present. This is the "safe to bill"
    /// predicate used by the cheapest-provider picker and the recommender
    /// to skip provider entries with placeholder pricing; per-bucket
    /// granular checks happen inside `calculate_cost`.
    pub fn is_complete(&self) -> bool {
        self.input_tokens.no_cache.is_some() && self.output_tokens.text.is_some()
    }
}

/// A routing table that maps incoming names to routing targets.
///
/// Used for both model routing and tool routing with separate instances.
pub trait RoutingTable: Send + Sync {
    /// Routes an incoming name to a routing target.
    ///
    /// `context` carries optional message metadata for content-aware routing.
    /// Non-API callers should pass [`RouteContext::default()`].
    fn route(
        &self,
        incoming_name: &str,
        context: &RouteContext,
    ) -> impl Future<Output = Result<RoutingTarget>> + Send;

    /// Resolves an incoming name to an ordered chain of routing targets.
    ///
    /// Implementors that only support single-target routing can rely on this
    /// default implementation, which wraps [`RoutingTable::route`] in a
    /// one-element chain.
    fn route_chain(
        &self,
        incoming_name: &str,
        context: &RouteContext,
    ) -> impl Future<Output = Result<Vec<RoutingTarget>>> + Send {
        async move { Ok(vec![self.route(incoming_name, context).await?]) }
    }

    /// Lists all configured routes.
    fn list_routes(&self) -> Vec<RouteEntry> {
        Vec::new()
    }
}

impl<T: RoutingTable> RoutingTable for Arc<T> {
    async fn route(&self, incoming_name: &str, context: &RouteContext) -> Result<RoutingTarget> {
        (**self).route(incoming_name, context).await
    }

    async fn route_chain(
        &self,
        incoming_name: &str,
        context: &RouteContext,
    ) -> Result<Vec<RoutingTarget>> {
        (**self).route_chain(incoming_name, context).await
    }

    fn list_routes(&self) -> Vec<RouteEntry> {
        (**self).list_routes()
    }
}

/// Strips ANSI escape sequences (CSI codes) from a string.
///
/// Model names and service IDs should never contain terminal formatting.
/// This function removes any `ESC[…X` sequences (where `X` is an ASCII
/// letter in `0x40..=0x7E`) to prevent ANSI leak from environment
/// variables, config values, or client payloads.
///
/// Malformed sequences (no final letter) are stripped up to end-of-string.
/// Non-ANSI bracket characters (e.g. `model[v2]`) are preserved.
pub fn strip_ansi_escapes(input: &str) -> String {
    let bytes = input.as_bytes();
    let len = bytes.len();
    let mut out = String::with_capacity(len);
    let mut i = 0;

    while i < len {
        if bytes[i] == 0x1b && i + 1 < len && bytes[i + 1] == b'[' {
            // Skip ESC + '[' and consume parameter bytes until the final byte
            // (an ASCII letter in 0x40..=0x7E) or end of string.
            i += 2;
            while i < len && !(0x40..=0x7E).contains(&bytes[i]) {
                i += 1;
            }
            if i < len {
                i += 1; // skip the final letter
            }
        } else {
            // The input is a valid UTF-8 `&str`, so indexing into a char
            // boundary always yields a valid character.
            let ch = input[i..].chars().next().unwrap_or_default();
            out.push(ch);
            i += ch.len_utf8();
        }
    }

    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn strip_ansi_removes_bold() {
        assert_eq!(
            strip_ansi_escapes("claude-opus-4-6\x1b[1m"),
            "claude-opus-4-6"
        );
    }

    #[test]
    fn strip_ansi_removes_bold_prefix() {
        assert_eq!(
            strip_ansi_escapes("\x1b[1mclaude-opus-4-6\x1b[0m"),
            "claude-opus-4-6"
        );
    }

    #[test]
    fn strip_ansi_noop_clean_string() {
        assert_eq!(strip_ansi_escapes("gpt-4o"), "gpt-4o");
    }

    #[test]
    fn strip_ansi_removes_color_codes() {
        assert_eq!(
            strip_ansi_escapes("\x1b[32mmodel-name\x1b[0m"),
            "model-name"
        );
    }

    #[test]
    fn strip_ansi_handles_empty_string() {
        assert_eq!(strip_ansi_escapes(""), "");
    }

    #[test]
    fn strip_ansi_preserves_brackets_without_esc() {
        // Literal brackets (no ESC prefix) should be preserved.
        assert_eq!(strip_ansi_escapes("model[v2]"), "model[v2]");
    }
}
