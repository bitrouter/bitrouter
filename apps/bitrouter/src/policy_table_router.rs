//! Config-driven per-request model routing — the `policy_table:` section.
//!
//! [`PolicyTableRouter`] is an ingress [`PromptTransform`] that picks the model
//! for each request from a static, operator-owned policy table instead of
//! taking the caller's requested model at face value. It is deterministic and
//! does no inference: it derives a coarse *fingerprint* of the request from the
//! canonical [`Prompt`] (the agent-loop step), looks the fingerprint up to get a
//! *tier*, maps the tier to a model id, and rewrites `prompt.model`.
//!
//! Two design points carry over from the sibling [`crate::claude_code`] router:
//!
//! - It lives in the app layer (not the SDK), because the decision needs the
//!   parsed [`Prompt`], which only exists above the SDK ingress seam, and its
//!   config is wired in [`crate::assemble`].
//! - It is idempotent and self-no-ops on anything it does not own. An explicit
//!   `provider:model` route — including the `claude-code:` subscription route
//!   the Claude Code router emits just before this one — always wins: such a
//!   request is left untouched. So is a request driven by a server-tool flow
//!   (it carries a bitrouter server-tool declaration, e.g. the `bitrouter/fusion`
//!   alias's injected tool), a request already on its tier's model, and one
//!   whose fingerprint resolves to no tier.
//!
//! The policy table is purely declarative and never mutated at runtime; it is
//! the kind of thing an operator keeps under version control.

use std::collections::HashMap;

use bitrouter_sdk::PromptTransform;
use bitrouter_sdk::config::PolicyTableConfig;
use bitrouter_sdk::language_model::types::{Content, Prompt, Role, Tool};

/// Ingress [`PromptTransform`] that rewrites `prompt.model` per a static policy
/// table keyed on a per-request fingerprint, with a hard tool-use guardrail.
///
/// Built from [`PolicyTableConfig`] via [`PolicyTableRouter::from_config`],
/// which returns `None` when the section defines no tiers (so an unconfigured
/// deployment registers nothing).
pub struct PolicyTableRouter {
    /// Tier name → model id the request is rewritten to.
    tiers: HashMap<String, String>,
    /// Request fingerprint → tier name.
    fingerprints: HashMap<String, String>,
    /// Tier for a fingerprint absent from `fingerprints`.
    default_tier: Option<String>,
    /// Guardrail target tier for tool-carrying requests whose chosen tier is not
    /// tool-safe.
    tool_use_tier: Option<String>,
    /// Tiers that handle tool calls reliably.
    tool_safe_tiers: Vec<String>,
}

impl PolicyTableRouter {
    /// Build a router from the `policy_table:` config, or `None` when the
    /// section is inert (no tiers defined) — mirroring
    /// `FusionAliasConfig::from_settings`, so an unconfigured deployment wires no
    /// transform.
    pub fn from_config(config: &PolicyTableConfig) -> Option<Self> {
        if config.tiers.is_empty() {
            return None;
        }
        Some(Self {
            tiers: config.tiers.clone(),
            fingerprints: config.fingerprints.clone(),
            default_tier: config.default_tier.clone(),
            tool_use_tier: config.tool_use_tier.clone(),
            tool_safe_tiers: config.tool_safe_tiers.clone(),
        })
    }

    /// Apply the policy table to a prompt, returning whether the model was
    /// rewritten. A no-op (returns `false`) when the model is already explicitly
    /// routed, when the request carries a bitrouter server-tool declaration,
    /// when the fingerprint resolves to no tier, or when the prompt is already on
    /// the resolved tier's model.
    pub fn apply(&self, prompt: &mut Prompt) -> bool {
        // An explicit `provider:model` route (including the `claude-code:`
        // subscription route) is the caller's deliberate choice and wins over
        // the soft policy table. Skipping it also makes re-application safe when
        // a tier resolves to a `provider:`-pinned model.
        if is_explicitly_routed(&prompt.model) {
            return false;
        }
        // A request carrying a bitrouter server-tool declaration is owned by
        // that server-tool flow — most visibly the `bitrouter/fusion` alias,
        // which (earlier in the transform chain) rewrites the model to a chosen
        // outer model and injects its `fusion` declaration. That outer model is
        // deliberate, so the policy table must not re-tier it.
        if carries_bitrouter_server_tool(prompt) {
            return false;
        }
        let Some(tier) = self.resolve_tier(prompt) else {
            return false;
        };
        let Some(model) = self.tiers.get(tier) else {
            // Unreachable for a config that passed `validate_policy_table`, but
            // a no-op is the safe fallback rather than a panic.
            return false;
        };
        if prompt.model == *model {
            return false;
        }
        prompt.model = model.clone();
        true
    }

    /// The tier this request resolves to: the fingerprint's tier (or
    /// `default_tier`), clamped up to `tool_use_tier` when the request carries
    /// tools and the chosen tier is not in `tool_safe_tiers`. `None` when the
    /// fingerprint maps to nothing and there is no default tier.
    fn resolve_tier(&self, prompt: &Prompt) -> Option<&str> {
        let fingerprint = Self::fingerprint(prompt);
        let mut tier = self
            .fingerprints
            .get(&fingerprint)
            .or(self.default_tier.as_ref())
            .map(String::as_str)?;
        // Hard tool-use guardrail: never route a tool-carrying request to a tier
        // not known tool-safe; clamp it up to the configured floor instead.
        if !prompt.tools.is_empty()
            && !self.tool_safe_tiers.iter().any(|t| t == tier)
            && let Some(floor) = self.tool_use_tier.as_deref()
        {
            tier = floor;
        }
        Some(tier)
    }

    /// A coarse fingerprint of the agent-loop step, derived purely from the
    /// prompt body (so it is stable regardless of the inbound protocol). It
    /// classifies the request by the model's *most recent* turn:
    ///
    /// - `after_<tool>` — the model's last turn called `<tool>` (the request is
    ///   most likely the follow-up that feeds the tool result back). This is the
    ///   common in-loop step.
    /// - `midstream` — the model's last turn was a plain reply with no tool call
    ///   (e.g. it answered, then the user sent a fresh instruction). Keying on
    ///   the *most recent* turn — rather than the last tool call anywhere in the
    ///   history — is what keeps a request that has moved past a tool turn from
    ///   being misread as the `after_<tool>` step.
    /// - `opening` — the model has taken no turn yet (the first request).
    ///
    /// When a turn makes several tool calls at once, the last call in the turn
    /// names the step. The fingerprint reads [`Content::ToolCall`] (whose name
    /// is always present) rather than a [`Content::ToolResult`] (whose name is
    /// wire-dependent and often absent).
    fn fingerprint(prompt: &Prompt) -> String {
        // Walk back to the model's most recent turn and classify by it.
        for message in prompt.messages.iter().rev() {
            if message.role != Role::Assistant {
                continue;
            }
            let last_call = message
                .content
                .iter()
                .rev()
                .find_map(|content| match content {
                    Content::ToolCall { name, .. } => Some(name.as_str()),
                    _ => None,
                });
            return match last_call {
                Some(name) => format!("after_{name}"),
                None => "midstream".to_string(),
            };
        }
        "opening".to_string()
    }
}

impl PromptTransform for PolicyTableRouter {
    fn apply(&self, prompt: &mut Prompt) {
        // The server applies every transform; discard the matched flag — a
        // non-matching request is a no-op. The fingerprint comes from the body
        // alone, so the header-aware `apply_with_headers` default (which
        // delegates here) needs no override.
        PolicyTableRouter::apply(self, prompt);
    }
}

/// Whether `model` already names an explicit upstream route. A `provider:model`
/// id triggers the routing table's Strategy-1 direct route (the same form the
/// `claude-code:` subscription route uses); the policy table defers to it.
fn is_explicitly_routed(model: &str) -> bool {
    model.contains(':')
}

/// Whether the request carries a bitrouter server-tool declaration — a
/// provider-defined tool in the bitrouter namespace, e.g. the `fusion` tool the
/// `bitrouter/fusion` alias injects (id `bitrouter.fusion`) or a caller's
/// `{"type":"bitrouter:advisor"}`. Such a request is driven by a server-tool
/// flow that already chose its outer model, so the policy table leaves it alone.
fn carries_bitrouter_server_tool(prompt: &Prompt) -> bool {
    prompt.tools.iter().any(|tool| match tool {
        Tool::ProviderDefined { id, name, .. } => {
            id.starts_with("bitrouter.") || is_bitrouter_namespaced(name)
        }
        Tool::Function { .. } => false,
    })
}

/// Whether `name` carries the explicit `bitrouter:` / `bitrouter.` namespace —
/// the documented `{"type":"bitrouter:<tool>"}` server-tool declaration form, as
/// opposed to a bare or foreign-namespaced tool a provider defines itself.
fn is_bitrouter_namespaced(name: &str) -> bool {
    name.split_once([':', '.'])
        .is_some_and(|(namespace, _)| namespace == "bitrouter")
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::language_model::types::{GenerationParams, Message, ProviderMetadata, Tool};

    /// A policy table with a cheap and a flagship tier: `opening` and tool-heavy
    /// steps stay flagship, a read step goes cheap, and only flagship is
    /// tool-safe.
    fn config() -> PolicyTableConfig {
        PolicyTableConfig {
            tiers: HashMap::from([
                ("cheap".to_string(), "vendor/cheap".to_string()),
                ("flagship".to_string(), "vendor/flagship".to_string()),
            ]),
            fingerprints: HashMap::from([
                ("opening".to_string(), "flagship".to_string()),
                ("after_read_file".to_string(), "cheap".to_string()),
            ]),
            default_tier: Some("flagship".to_string()),
            tool_use_tier: Some("flagship".to_string()),
            tool_safe_tiers: vec!["flagship".to_string()],
        }
    }

    fn router() -> PolicyTableRouter {
        PolicyTableRouter::from_config(&config()).expect("tiers are configured")
    }

    fn prompt(model: &str) -> Prompt {
        Prompt {
            model: model.to_string(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

    /// An assistant message whose only content is a call to `tool`.
    fn assistant_calls(tool: &str) -> Message {
        Message {
            role: Role::Assistant,
            content: vec![Content::ToolCall {
                id: format!("call_{tool}"),
                name: tool.to_string(),
                arguments: "{}".to_string(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        }
    }

    /// An assistant message that is a plain text reply (no tool call) — a
    /// completed model turn.
    fn assistant_text(text: &str) -> Message {
        Message::text(Role::Assistant, text)
    }

    /// An assistant message that calls several tools in one turn, in order.
    fn assistant_calls_multi(tools: &[&str]) -> Message {
        Message {
            role: Role::Assistant,
            content: tools
                .iter()
                .map(|tool| Content::ToolCall {
                    id: format!("call_{tool}"),
                    name: tool.to_string(),
                    arguments: "{}".to_string(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: ProviderMetadata::new(),
                })
                .collect(),
        }
    }

    /// A minimal function tool, so a request "carries tools".
    fn a_tool() -> Tool {
        Tool::Function {
            name: "read_file".to_string(),
            description: None,
            parameters: serde_json::json!({"type": "object"}),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    /// The provider-defined declaration the `bitrouter/fusion` alias injects.
    fn fusion_declaration() -> Tool {
        Tool::ProviderDefined {
            id: "bitrouter.fusion".to_string(),
            name: "fusion".to_string(),
            args: serde_json::json!({}),
            provider_metadata: ProviderMetadata::new(),
        }
    }

    /// Drive the router over a constructed prompt and return the routed model.
    fn route(model: &str, messages: Vec<Message>, tools: Vec<Tool>) -> String {
        let mut p = prompt(model);
        p.messages = messages;
        p.tools = tools;
        router().apply(&mut p);
        p.model
    }

    #[test]
    fn opening_request_routes_to_its_tier() {
        // No model turn yet → `opening` → flagship.
        assert_eq!(
            route("inbound", vec![user("fix the bug")], vec![]),
            "vendor/flagship"
        );
    }

    #[test]
    fn after_tool_step_routes_to_its_tier() {
        // The model last called `read_file` → `after_read_file` → cheap.
        assert_eq!(
            route(
                "inbound",
                vec![user("fix the bug"), assistant_calls("read_file")],
                vec![],
            ),
            "vendor/cheap"
        );
    }

    #[test]
    fn unmapped_fingerprint_falls_back_to_default_tier() {
        // `after_grep` is not mapped → default_tier (flagship).
        assert_eq!(
            route(
                "inbound",
                vec![user("fix the bug"), assistant_calls("grep")],
                vec![],
            ),
            "vendor/flagship"
        );
    }

    #[test]
    fn tool_use_guardrail_clamps_a_non_tool_safe_tier() {
        // `after_read_file` would route to cheap, but the request carries tools
        // and cheap is not tool-safe → clamped up to the tool_use_tier
        // (flagship). The guardrail is the key safety property.
        assert_eq!(
            route(
                "inbound",
                vec![user("fix the bug"), assistant_calls("read_file")],
                vec![a_tool()],
            ),
            "vendor/flagship"
        );
    }

    #[test]
    fn explicit_provider_route_is_left_untouched() {
        // A `provider:model` pin (and the `claude-code:` subscription route) is
        // the caller's deliberate choice and is never re-tiered.
        assert_eq!(
            route("vendor:exact-model", vec![user("hi")], vec![]),
            "vendor:exact-model"
        );
        assert_eq!(
            route("claude-code:claude-opus-4-8", vec![user("hi")], vec![]),
            "claude-code:claude-opus-4-8"
        );
    }

    #[test]
    fn idempotent_on_second_application() {
        // Applying twice must not double-route: the second pass is already on
        // the tier's model and no-ops.
        let mut p = prompt("inbound");
        p.messages = vec![user("fix the bug")];
        assert!(router().apply(&mut p), "first pass routes");
        assert_eq!(p.model, "vendor/flagship");
        assert!(!router().apply(&mut p), "second pass is a no-op");
        assert_eq!(p.model, "vendor/flagship");
    }

    #[test]
    fn unmapped_fingerprint_without_default_is_a_noop() {
        // No default_tier and an unmapped fingerprint → the caller's model is
        // left as-is.
        let cfg = PolicyTableConfig {
            tiers: HashMap::from([("cheap".to_string(), "vendor/cheap".to_string())]),
            fingerprints: HashMap::from([("opening".to_string(), "cheap".to_string())]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
        };
        let r = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut p = prompt("inbound");
        p.messages = vec![user("hi"), assistant_calls("grep")];
        assert!(!r.apply(&mut p));
        assert_eq!(p.model, "inbound");
    }

    #[test]
    fn from_config_is_none_when_no_tiers() {
        assert!(PolicyTableRouter::from_config(&PolicyTableConfig::default()).is_none());
    }

    #[test]
    fn a_completed_turn_past_a_tool_call_is_midstream_not_after_tool() {
        // The model called `read_file`, then replied with text, then the user
        // sent a fresh instruction. The most recent model turn is the text
        // reply, so this is `midstream` (→ default flagship), NOT the stale
        // `after_read_file` step (→ cheap).
        let routed = route(
            "inbound",
            vec![
                user("fix the bug"),
                assistant_calls("read_file"),
                assistant_text("here is what I found"),
                user("now refactor it"),
            ],
            vec![],
        );
        assert_eq!(routed, "vendor/flagship");
        assert_ne!(routed, "vendor/cheap");
    }

    #[test]
    fn parallel_tool_calls_use_the_last_call_in_the_turn() {
        // A turn calling [grep, read_file] keys on the last call (`read_file` →
        // cheap); the unmapped `after_grep` would have fallen to default flagship,
        // so this proves the last-in-turn call names the step.
        assert_eq!(
            route(
                "inbound",
                vec![user("fix"), assistant_calls_multi(&["grep", "read_file"])],
                vec![],
            ),
            "vendor/cheap"
        );
    }

    #[test]
    fn colon_form_tier_target_is_idempotent() {
        // A tier that resolves to a `provider:model` (colon) id: the first pass
        // routes to it, and the second pass skips it as an explicit route.
        let cfg = PolicyTableConfig {
            tiers: HashMap::from([("flagship".to_string(), "vendor:exact".to_string())]),
            fingerprints: HashMap::from([("opening".to_string(), "flagship".to_string())]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
        };
        let r = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut p = prompt("inbound");
        p.messages = vec![user("hi")];
        assert!(r.apply(&mut p), "first pass routes");
        assert_eq!(p.model, "vendor:exact");
        assert!(!r.apply(&mut p), "second pass skips the explicit route");
        assert_eq!(p.model, "vendor:exact");
    }

    #[test]
    fn disabled_guardrail_lets_a_tool_request_route_cheap() {
        // With no `tool_use_tier`, the guardrail is off: a tool-carrying request
        // routes by fingerprint like any other (here `after_read_file` → cheap).
        let cfg = PolicyTableConfig {
            tiers: HashMap::from([
                ("cheap".to_string(), "vendor/cheap".to_string()),
                ("flagship".to_string(), "vendor/flagship".to_string()),
            ]),
            fingerprints: HashMap::from([("after_read_file".to_string(), "cheap".to_string())]),
            default_tier: Some("flagship".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
        };
        let r = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut p = prompt("inbound");
        p.messages = vec![user("fix"), assistant_calls("read_file")];
        p.tools = vec![a_tool()];
        assert!(r.apply(&mut p));
        assert_eq!(p.model, "vendor/cheap");
    }

    #[test]
    fn fusion_declaration_is_left_untouched() {
        // A request carrying the fusion alias's injected declaration is owned by
        // the fusion flow; the policy table must not re-tier its outer model,
        // even though the model is colonless and the request carries tools.
        assert_eq!(
            route(
                "vendor/fusion-outer",
                vec![user("compare these")],
                vec![fusion_declaration()]
            ),
            "vendor/fusion-outer"
        );
    }
}
