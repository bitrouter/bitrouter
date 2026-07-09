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
use std::fmt;
use std::sync::Arc;

use bitrouter_sdk::config::{PolicyKeyStrategy, PolicyTableConfig};
use bitrouter_sdk::language_model::types::{Content, Prompt, Role, Tool};
use bitrouter_sdk::{HeaderMap, PromptTransform};

use crate::adequacy::AdequacyLedger;
use crate::workflow_state::online::OnlineWorkflowState;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecisionReason {
    StaticTable,
    ExplorationTrial,
    ExplorationLocked,
    AdequacyPin,
    ToolGuardrail,
    NoMatch,
}

impl PolicyDecisionReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::StaticTable => "static_table",
            Self::ExplorationTrial => "exploration_trial",
            Self::ExplorationLocked => "exploration_locked",
            Self::AdequacyPin => "adequacy_pin",
            Self::ToolGuardrail => "tool_guardrail",
            Self::NoMatch => "no_match",
        }
    }
}

impl fmt::Display for PolicyDecisionReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

#[derive(Debug, Clone)]
pub struct PolicyDecision {
    pub key_strategy: PolicyKeyStrategy,
    pub request_key: String,
    pub legacy_fingerprint: String,
    pub workflow_state_kind: String,
    pub static_tier: Option<String>,
    pub selected_tier: Option<String>,
    pub selected_model: Option<String>,
    pub reason: PolicyDecisionReason,
    pub pinned: bool,
    pub locked: bool,
    pub trialed: bool,
}

/// The resolved, immutable policy spec — the fingerprint→tier→model table plus
/// the guardrail and (for adaptive routing) the escalation tier and a reverse
/// model→tier index. Shared via [`Arc`] between the router (which reads it on
/// the ingress hot path) and the adequacy observer (which recomputes the
/// fingerprint and maps the served model back to a tier).
pub struct PolicyTable {
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
    /// Tier a pinned fingerprint escalates to (adequacy.escalation_tier, else
    /// default_tier). `None` when neither is configured.
    escalation_tier: Option<String>,
    /// Cheap tier exploration trials toward (adequacy.explore_tier). `None` when
    /// exploration is off.
    explore_tier: Option<String>,
    /// Whether aggressive downgrade discovery is enabled.
    exploration_enabled: bool,
    /// Whether the opening turn is eligible for exploration.
    explore_opening: bool,
    /// Future task-reward guardrail for opening downgrades.
    min_semantic_successes_for_opening: u32,
    /// Reverse index model id → tier name, for mapping a served model back to
    /// its tier at observe time.
    model_to_tier: HashMap<String, String>,
    /// The request key family used by `fingerprints` and adequacy state.
    key_strategy: PolicyKeyStrategy,
}

impl PolicyTable {
    /// Build the shared spec from config, or `None` when the section is inert
    /// (no tiers defined).
    pub fn from_config(config: &PolicyTableConfig) -> Option<Arc<Self>> {
        if config.tiers.is_empty() {
            return None;
        }
        let model_to_tier = config
            .tiers
            .iter()
            .map(|(tier, model)| (model.clone(), tier.clone()))
            .collect();
        let escalation_tier = config
            .adequacy
            .escalation_tier
            .clone()
            .or_else(|| config.default_tier.clone());
        // Exploration is live only when enabled, a target tier is set, and there
        // is an escalation tier to be a candidate against.
        let exploration_enabled = config.adequacy.explore_enabled
            && config.adequacy.explore_tier.is_some()
            && escalation_tier.is_some();
        Some(Arc::new(Self {
            tiers: config.tiers.clone(),
            fingerprints: config.fingerprints.clone(),
            default_tier: config.default_tier.clone(),
            tool_use_tier: config.tool_use_tier.clone(),
            tool_safe_tiers: config.tool_safe_tiers.clone(),
            escalation_tier,
            explore_tier: config.adequacy.explore_tier.clone(),
            exploration_enabled,
            explore_opening: config.adequacy.explore_opening,
            min_semantic_successes_for_opening: config
                .adequacy
                .min_semantic_successes_for_opening
                .max(1),
            model_to_tier,
            key_strategy: config.key_strategy,
        }))
    }

    /// The tier a fingerprint maps to (or `default_tier`), before any guardrail
    /// or escalation. `None` when unmapped and no default tier is set.
    fn tier_for_fingerprint(&self, fingerprint: &str) -> Option<&str> {
        self.fingerprints
            .get(fingerprint)
            .or(self.default_tier.as_ref())
            .map(String::as_str)
    }

    /// Apply the hard tool-use guardrail: a tool-carrying request whose `tier` is
    /// not tool-safe is clamped up to `tool_use_tier`. Returns the effective tier.
    fn guardrail<'a>(&'a self, tier: &'a str, prompt: &Prompt) -> &'a str {
        self.guardrail_with_status(tier, prompt).0
    }

    fn guardrail_with_status<'a>(&'a self, tier: &'a str, prompt: &Prompt) -> (&'a str, bool) {
        if !prompt.tools.is_empty()
            && !self.tool_safe_tiers.iter().any(|t| t == tier)
            && let Some(floor) = self.tool_use_tier.as_deref()
        {
            return (floor, floor != tier);
        }
        (tier, false)
    }

    /// The model id a tier routes to.
    fn model_of_tier(&self, tier: &str) -> Option<&str> {
        self.tiers.get(tier).map(String::as_str)
    }

    /// The tier a served model id belongs to (reverse of [`Self::model_of_tier`]).
    /// Used by the adequacy observer to map an outcome back to a tier.
    pub(crate) fn tier_of_model(&self, model: &str) -> Option<&str> {
        self.model_to_tier.get(model).map(String::as_str)
    }

    /// The tier a pinned fingerprint escalates to. Used by the router (to apply a
    /// pin) and the observer (to tell a downgrade from the escalation tier).
    pub(crate) fn escalation_tier(&self) -> Option<&str> {
        self.escalation_tier.as_deref()
    }

    pub(crate) fn static_tier_with_headers(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
    ) -> Option<&str> {
        let key = self.request_key(prompt, headers);
        self.static_tier_for(key.as_str(), prompt)
    }

    pub(crate) fn request_key(&self, prompt: &Prompt, headers: &HeaderMap) -> String {
        match self.key_strategy {
            PolicyKeyStrategy::LegacyFingerprint => Self::fingerprint(prompt),
            PolicyKeyStrategy::WorkflowState => OnlineWorkflowState::from_headers(headers, prompt)
                .routing_key()
                .to_string(),
        }
    }

    pub(crate) fn key_strategy(&self) -> PolicyKeyStrategy {
        self.key_strategy
    }

    /// [`Self::static_tier`] for an already-computed fingerprint.
    fn static_tier_for(&self, fingerprint: &str, prompt: &Prompt) -> Option<&str> {
        let tier = self.tier_for_fingerprint(fingerprint)?;
        Some(self.guardrail(tier, prompt))
    }

    /// The cheap tier exploration trials toward (raw; gate on
    /// [`Self::exploration_enabled`]).
    pub(crate) fn explore_tier(&self) -> Option<&str> {
        self.explore_tier.as_deref()
    }

    /// Whether aggressive downgrade discovery is live.
    pub(crate) fn exploration_enabled(&self) -> bool {
        self.exploration_enabled
    }

    fn can_explore_opening(&self) -> bool {
        self.explore_opening && self.min_semantic_successes_for_opening >= 1
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
    pub fn fingerprint(prompt: &Prompt) -> String {
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

/// Ingress [`PromptTransform`] that rewrites `prompt.model` per a [`PolicyTable`]
/// keyed on a per-request fingerprint, with a hard tool-use guardrail.
///
/// When an [`AdequacyLedger`] is attached, a fingerprint that the ledger has
/// *pinned* (because the downgrade kept failing) is routed to the table's
/// escalation tier instead of the cheap one — adaptive, self-correcting routing.
/// Without a ledger the router is exactly the static table.
///
/// Build it from [`PolicyTableConfig`] via [`PolicyTableRouter::from_config`]
/// (static, `None` when no tiers are defined) or [`PolicyTableRouter::new`] (with
/// a shared table and optional ledger, for the adaptive wiring).
pub struct PolicyTableRouter {
    table: Arc<PolicyTable>,
    ledger: Option<Arc<AdequacyLedger>>,
}

impl PolicyTableRouter {
    /// Build a static router from the `policy_table:` config, or `None` when the
    /// section is inert (no tiers defined) — mirroring
    /// `FusionAliasConfig::from_settings`, so an unconfigured deployment wires no
    /// transform. No adequacy ledger is attached.
    pub fn from_config(config: &PolicyTableConfig) -> Option<Self> {
        PolicyTable::from_config(config).map(|table| Self {
            table,
            ledger: None,
        })
    }

    /// Build a router over a shared [`PolicyTable`] and an optional
    /// [`AdequacyLedger`] (the adaptive wiring).
    pub fn new(table: Arc<PolicyTable>, ledger: Option<Arc<AdequacyLedger>>) -> Self {
        Self { table, ledger }
    }

    /// Apply the policy table to a prompt, returning whether the model was
    /// rewritten. A no-op (returns `false`) when the model is already explicitly
    /// routed, when the request carries a bitrouter server-tool declaration,
    /// when the fingerprint resolves to no tier, or when the prompt is already on
    /// the resolved tier's model.
    pub fn apply(&self, prompt: &mut Prompt) -> bool {
        self.route_prompt(prompt, &HeaderMap::new())
    }

    pub fn decision_for(&self, prompt: &Prompt, headers: &HeaderMap) -> PolicyDecision {
        let online = OnlineWorkflowState::from_headers(headers, prompt);
        let legacy_fingerprint = online.legacy_fingerprint().to_string();
        let request_key = match self.table.key_strategy() {
            PolicyKeyStrategy::LegacyFingerprint => legacy_fingerprint.clone(),
            PolicyKeyStrategy::WorkflowState => online.routing_key().to_string(),
        };
        let mut decision = PolicyDecision {
            key_strategy: self.table.key_strategy(),
            request_key,
            legacy_fingerprint,
            workflow_state_kind: online.ir.state_kind.to_string(),
            static_tier: None,
            selected_tier: None,
            selected_model: None,
            reason: PolicyDecisionReason::NoMatch,
            pinned: false,
            locked: false,
            trialed: false,
        };

        if is_explicitly_routed(&prompt.model) || carries_bitrouter_server_tool(prompt) {
            return decision;
        }

        let Some(raw_static_tier) = self.table.tier_for_fingerprint(&decision.request_key) else {
            return decision;
        };
        decision.static_tier = Some(raw_static_tier.to_string());
        let (mut selected_tier, static_clamped) =
            self.table.guardrail_with_status(raw_static_tier, prompt);
        decision.reason = if static_clamped {
            PolicyDecisionReason::ToolGuardrail
        } else {
            PolicyDecisionReason::StaticTable
        };

        if let Some(ledger) = &self.ledger {
            decision.pinned = ledger.is_pinned(&decision.request_key);
            decision.locked = ledger.is_locked(&decision.request_key);
            if decision.pinned {
                if let Some(escalation) = self.table.escalation_tier() {
                    (selected_tier, _) = self.table.guardrail_with_status(escalation, prompt);
                    decision.reason = PolicyDecisionReason::AdequacyPin;
                }
            } else if self.table.exploration_enabled()
                && Some(selected_tier) == self.table.escalation_tier()
                && self.exploration_allowed_for(&decision)
                && let Some(explore) = self.table.explore_tier()
            {
                let should_trial = ledger.should_trial(&decision.request_key);
                if decision.locked || should_trial {
                    let (guarded_explore, explore_clamped) =
                        self.table.guardrail_with_status(explore, prompt);
                    selected_tier = guarded_explore;
                    decision.trialed = should_trial && !decision.locked;
                    decision.reason = if explore_clamped {
                        PolicyDecisionReason::ToolGuardrail
                    } else if decision.locked {
                        PolicyDecisionReason::ExplorationLocked
                    } else {
                        PolicyDecisionReason::ExplorationTrial
                    };
                }
            }
        }

        decision.selected_tier = Some(selected_tier.to_string());
        decision.selected_model = self
            .table
            .model_of_tier(selected_tier)
            .map(ToString::to_string);
        if decision.selected_model.is_none() {
            decision.reason = PolicyDecisionReason::NoMatch;
        }
        decision
    }

    fn exploration_allowed_for(&self, decision: &PolicyDecision) -> bool {
        if decision.legacy_fingerprint == "opening" || decision.workflow_state_kind == "opening" {
            return self.table.can_explore_opening();
        }
        true
    }

    fn route_prompt(&self, prompt: &mut Prompt, headers: &HeaderMap) -> bool {
        let decision = self.decision_for(prompt, headers);
        let request_id = headers
            .get("x-bitrouter-request-id")
            .and_then(|value| value.to_str().ok())
            .unwrap_or("-");
        tracing::info!(
            request_id,
            key_strategy = ?decision.key_strategy,
            request_key = %decision.request_key,
            legacy_fingerprint = %decision.legacy_fingerprint,
            workflow_state = %decision.workflow_state_kind,
            static_tier = ?decision.static_tier,
            selected_tier = ?decision.selected_tier,
            selected_model = ?decision.selected_model,
            reason = %decision.reason,
            pinned = decision.pinned,
            locked = decision.locked,
            trialed = decision.trialed,
            "policy routing decision"
        );
        let Some(model) = decision.selected_model else {
            return false;
        };
        if prompt.model == model {
            return false;
        }
        prompt.model = model;
        true
    }
}

impl PromptTransform for PolicyTableRouter {
    fn apply(&self, prompt: &mut Prompt) {
        PolicyTableRouter::apply(self, prompt);
    }

    fn apply_with_headers(&self, prompt: &mut Prompt, headers: &HeaderMap) {
        self.route_prompt(prompt, headers);
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
    use crate::adequacy::{InadequacyCause, Outcome};
    use crate::workflow_state::ir::{HarnessId, ProtocolKind};
    use crate::workflow_state::online::OnlineWorkflowState;
    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::config::PolicyKeyStrategy;
    use bitrouter_sdk::language_model::types::{GenerationParams, Message, ProviderMetadata, Tool};
    use http::HeaderValue;

    /// A policy table with a cheap and a flagship tier: `opening` and tool-heavy
    /// steps stay flagship, a read step goes cheap, and only flagship is
    /// tool-safe.
    fn config() -> PolicyTableConfig {
        PolicyTableConfig {
            key_strategy: Default::default(),
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
            adequacy: Default::default(),
        }
    }

    fn router() -> PolicyTableRouter {
        PolicyTableRouter::from_config(&config()).expect("tiers are configured")
    }

    fn claude_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219,tools-2024-05-16"),
        );
        headers
    }

    /// `config()` with online adequacy learning enabled, escalating pinned
    /// fingerprints to the flagship tier.
    fn config_with_escalation() -> PolicyTableConfig {
        let mut cfg = config();
        cfg.adequacy.enabled = true;
        cfg.adequacy.escalation_tier = Some("flagship".to_string());
        cfg
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
            key_strategy: Default::default(),
            tiers: HashMap::from([("cheap".to_string(), "vendor/cheap".to_string())]),
            fingerprints: HashMap::from([("opening".to_string(), "cheap".to_string())]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
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
            key_strategy: Default::default(),
            tiers: HashMap::from([("flagship".to_string(), "vendor:exact".to_string())]),
            fingerprints: HashMap::from([("opening".to_string(), "flagship".to_string())]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
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
            key_strategy: Default::default(),
            tiers: HashMap::from([
                ("cheap".to_string(), "vendor/cheap".to_string()),
                ("flagship".to_string(), "vendor/flagship".to_string()),
            ]),
            fingerprints: HashMap::from([("after_read_file".to_string(), "cheap".to_string())]),
            default_tier: Some("flagship".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
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

    // ---- adaptive routing (the adequacy ledger) ----

    /// A read step prompt — fingerprints to `after_read_file` (→ cheap statically).
    fn read_step() -> Vec<Message> {
        vec![user("fix the bug"), assistant_calls("read_file")]
    }

    fn route_with(router: &PolicyTableRouter, messages: Vec<Message>) -> String {
        let mut p = prompt("inbound");
        p.messages = messages;
        router.apply(&mut p);
        p.model
    }

    fn route_with_headers(
        router: &PolicyTableRouter,
        messages: Vec<Message>,
        headers: &HeaderMap,
    ) -> String {
        let mut p = prompt("inbound");
        p.messages = messages;
        router.apply_with_headers(&mut p, headers);
        p.model
    }

    #[test]
    fn workflow_state_key_strategy_uses_ir_key_for_lookup() {
        let mut cfg = config();
        cfg.key_strategy = PolicyKeyStrategy::WorkflowState;
        cfg.fingerprints.clear();
        cfg.default_tier = Some("flagship".to_string());

        let mut probe = prompt("inbound");
        probe.messages = vec![user("fix"), assistant_calls("Bash")];
        let headers = claude_headers();
        let key = OnlineWorkflowState::from_prompt(
            &headers,
            &probe,
            Some(HarnessId::ClaudeCode),
            ProtocolKind::Messages,
        )
        .routing_key()
        .to_string();
        cfg.fingerprints.insert(key, "cheap".to_string());

        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        assert_eq!(
            route_with_headers(
                &router,
                vec![user("fix"), assistant_calls("Bash")],
                &headers
            ),
            "vendor/cheap"
        );
    }

    #[test]
    fn decision_reason_static_table() {
        let router = router();
        let mut p = prompt("inbound");
        p.messages = read_step();

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::StaticTable);
        assert_eq!(decision.static_tier.as_deref(), Some("cheap"));
        assert_eq!(decision.selected_tier.as_deref(), Some("cheap"));
        assert_eq!(decision.selected_model.as_deref(), Some("vendor/cheap"));
    }

    #[test]
    fn decision_reason_tool_guardrail() {
        let router = router();
        let mut p = prompt("inbound");
        p.messages = read_step();
        p.tools = vec![a_tool()];

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::ToolGuardrail);
        assert_eq!(decision.static_tier.as_deref(), Some("cheap"));
        assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
    }

    #[test]
    fn decision_reason_no_match() {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([("cheap".to_string(), "vendor/cheap".to_string())]),
            fingerprints: HashMap::from([("opening".to_string(), "cheap".to_string())]),
            default_tier: None,
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        let router = PolicyTableRouter::from_config(&cfg).expect("configured");
        let mut p = prompt("inbound");
        p.messages = vec![user("hi"), assistant_calls("grep")];

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::NoMatch);
        assert_eq!(decision.selected_tier, None);
        assert_eq!(decision.selected_model, None);
    }

    #[tokio::test]
    async fn decision_reason_adequacy_pin() {
        let table = PolicyTable::from_config(&config_with_escalation()).expect("configured");
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let router = PolicyTableRouter::new(table, Some(ledger.clone()));
        ledger
            .observe(
                "after_read_file",
                Outcome::StaticDowngrade {
                    cause: InadequacyCause::ProviderPermanent,
                },
            )
            .await;
        let mut p = prompt("inbound");
        p.messages = read_step();

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::AdequacyPin);
        assert!(decision.pinned);
        assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
    }

    #[tokio::test]
    async fn decision_reason_exploration_trial() {
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 2));
        let non_trial = || Outcome::Exploration {
            trialed: false,
            cause: InadequacyCause::None,
        };
        ledger.observe("opening", non_trial()).await;
        ledger.observe("opening", non_trial()).await;
        let router = exploring_router(ledger);
        let mut p = prompt("inbound");
        p.messages = vec![user("start")];

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::ExplorationTrial);
        assert!(decision.trialed);
        assert_eq!(decision.selected_tier.as_deref(), Some("cheap"));
    }

    #[tokio::test]
    async fn opening_is_not_explored_without_explicit_opt_in() {
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 1, 1));
        ledger
            .observe(
                "opening",
                Outcome::Exploration {
                    trialed: false,
                    cause: InadequacyCause::None,
                },
            )
            .await;
        let table = PolicyTable::from_config(&config_with_exploration()).expect("configured");
        let router = PolicyTableRouter::new(table, Some(ledger));

        assert_eq!(route_with(&router, vec![user("start")]), "vendor/flagship");
    }

    #[tokio::test]
    async fn decision_reason_exploration_locked() {
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 1));
        ledger.observe("opening", trial_ok()).await;
        let router = exploring_router(ledger);
        let mut p = prompt("inbound");
        p.messages = vec![user("start")];

        let decision = router.decision_for(&p, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::ExplorationLocked);
        assert!(decision.locked);
        assert_eq!(decision.selected_tier.as_deref(), Some("cheap"));
    }

    #[tokio::test]
    async fn workflow_state_key_strategy_uses_ir_key_for_ledger_pins() {
        let mut cfg = config_with_escalation();
        cfg.key_strategy = PolicyKeyStrategy::WorkflowState;
        cfg.fingerprints.clear();
        cfg.default_tier = Some("flagship".to_string());

        let mut probe = prompt("inbound");
        probe.messages = read_step();
        let headers = claude_headers();
        let key = OnlineWorkflowState::from_prompt(
            &headers,
            &probe,
            Some(HarnessId::ClaudeCode),
            ProtocolKind::Messages,
        )
        .routing_key()
        .to_string();
        cfg.fingerprints.insert(key.clone(), "cheap".to_string());

        let table = PolicyTable::from_config(&cfg).expect("configured");
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let router = PolicyTableRouter::new(table, Some(ledger.clone()));

        assert_eq!(
            route_with_headers(&router, read_step(), &headers),
            "vendor/cheap"
        );

        ledger
            .observe(
                &key,
                Outcome::StaticDowngrade {
                    cause: InadequacyCause::ProviderPermanent,
                },
            )
            .await;

        assert_eq!(
            route_with_headers(&router, read_step(), &headers),
            "vendor/flagship",
            "workflow-state ledger pin escalates the matching IR key"
        );
    }

    #[tokio::test]
    async fn a_pinned_fingerprint_escalates_over_the_static_downgrade() {
        let table = PolicyTable::from_config(&config_with_escalation()).expect("configured");
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let router = PolicyTableRouter::new(table, Some(ledger.clone()));

        // Before any failure, the static table downgrades `after_read_file`.
        assert_eq!(route_with(&router, read_step()), "vendor/cheap");

        // One inadequate outcome pins the fingerprint (threshold 1).
        ledger
            .observe(
                "after_read_file",
                Outcome::StaticDowngrade {
                    cause: InadequacyCause::ProviderPermanent,
                },
            )
            .await;

        // Now the same step escalates to the flagship (escalation) tier.
        assert_eq!(
            route_with(&router, read_step()),
            "vendor/flagship",
            "a pinned fingerprint escalates over the static downgrade"
        );
    }

    #[tokio::test]
    async fn escalation_is_scoped_to_the_pinned_fingerprint() {
        let table = PolicyTable::from_config(&config_with_escalation()).expect("configured");
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let router = PolicyTableRouter::new(table, Some(ledger.clone()));

        ledger
            .observe(
                "after_read_file",
                Outcome::StaticDowngrade {
                    cause: InadequacyCause::ProviderPermanent,
                },
            )
            .await; // pin only this fingerprint

        // A different downgraded step is unaffected: map `after_grep` → cheap.
        // (It is unmapped here, so it falls to default flagship anyway; assert the
        // pinned one escalates while an opening request stays flagship as before.)
        assert_eq!(route_with(&router, read_step()), "vendor/flagship"); // pinned
        assert_eq!(
            route_with(&router, vec![user("start")]),
            "vendor/flagship" // `opening` was always flagship; unchanged
        );
    }

    #[tokio::test]
    async fn no_ledger_means_no_escalation() {
        // Built with `from_config` (no ledger): observing has nothing to read, so
        // routing stays exactly static.
        let router = PolicyTableRouter::from_config(&config_with_escalation()).expect("configured");
        assert_eq!(route_with(&router, read_step()), "vendor/cheap");
    }

    // ---- aggressive exploration (downgrade discovery) ----

    /// `config()` with exploration on: `opening` → flagship (the escalation tier)
    /// is an exploration candidate, trialed toward the cheap tier.
    fn config_with_exploration() -> PolicyTableConfig {
        let mut cfg = config_with_escalation();
        cfg.adequacy.explore_enabled = true;
        cfg.adequacy.explore_tier = Some("cheap".to_string());
        cfg.adequacy.explore_interval = 2;
        cfg.adequacy.explore_threshold = 2;
        cfg
    }

    fn config_with_opening_exploration() -> PolicyTableConfig {
        let mut cfg = config_with_exploration();
        cfg.adequacy.explore_opening = true;
        cfg
    }

    fn exploring_router(ledger: Arc<AdequacyLedger>) -> PolicyTableRouter {
        let table =
            PolicyTable::from_config(&config_with_opening_exploration()).expect("configured");
        PolicyTableRouter::new(table, Some(ledger))
    }

    fn trial_ok() -> Outcome {
        Outcome::Exploration {
            trialed: true,
            cause: InadequacyCause::None,
        }
    }

    #[tokio::test]
    async fn an_unseen_candidate_stays_on_the_escalation_tier() {
        // No learned state yet → `opening` routes to its static (escalation) tier.
        let router = exploring_router(Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 2)));
        assert_eq!(route_with(&router, vec![user("start")]), "vendor/flagship");
    }

    #[tokio::test]
    async fn a_due_trial_routes_a_candidate_to_the_explore_tier() {
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 2));
        // Two non-trial observations advance the cadence so a trial is due.
        let non_trial = || Outcome::Exploration {
            trialed: false,
            cause: InadequacyCause::None,
        };
        ledger.observe("opening", non_trial()).await;
        ledger.observe("opening", non_trial()).await;
        let router = exploring_router(ledger);
        assert_eq!(
            route_with(&router, vec![user("start")]),
            "vendor/cheap",
            "a candidate due for a trial routes to the explore tier"
        );
    }

    #[tokio::test]
    async fn a_locked_candidate_routes_to_the_explore_tier() {
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 2));
        ledger.observe("opening", trial_ok()).await; // 1 adequate trial
        ledger.observe("opening", trial_ok()).await; // 2 → locked
        let router = exploring_router(ledger);
        assert_eq!(route_with(&router, vec![user("start")]), "vendor/cheap");
    }

    #[tokio::test]
    async fn a_pinned_candidate_escalates_over_exploration() {
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 2));
        // A failed trial pins the candidate (safety wins).
        ledger
            .observe(
                "opening",
                Outcome::Exploration {
                    trialed: true,
                    cause: InadequacyCause::ProviderPermanent,
                },
            )
            .await;
        let router = exploring_router(ledger);
        assert_eq!(
            route_with(&router, vec![user("start")]),
            "vendor/flagship",
            "a pin overrides exploration"
        );
    }

    #[tokio::test]
    async fn exploration_respects_the_tool_use_guardrail() {
        // A locked candidate that carries tools is clamped back up by the
        // guardrail: a tool request is never downgraded below the tool-safe tier,
        // even when exploration has locked the cheap tier.
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 1));
        ledger.observe("opening", trial_ok()).await; // locks (threshold 1)
        let router = exploring_router(ledger);
        let mut p = prompt("inbound");
        p.messages = vec![user("start")];
        p.tools = vec![a_tool()];
        router.apply(&mut p);
        assert_eq!(p.model, "vendor/flagship", "guardrail clamps the trial");
    }

    // ---- fingerprint parity through the real Chat Completions wire ----
    //
    // These exercise the full ingress path a harness drives — an OpenAI Chat
    // Completions request body parsed by the daemon's inbound adapter into the
    // canonical `Prompt`, then fingerprinted — and assert the agent-loop step
    // label, so the native router keys requests the same way an external
    // fingerprinting proxy would.

    fn fingerprint_of(body: serde_json::Value) -> String {
        use bitrouter_sdk::language_model::inbound_adapter_for;
        use bitrouter_sdk::language_model::types::ApiProtocol;
        let adapter =
            inbound_adapter_for(&ApiProtocol::ChatCompletions).expect("chat completions adapter");
        let prompt = adapter.parse_request(body).expect("parse request");
        PolicyTable::fingerprint(&prompt)
    }

    #[test]
    fn opening_request_fingerprints_through_the_wire() {
        // System + first user turn, no assistant yet → the opening step.
        assert_eq!(
            fingerprint_of(serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "system", "content": "You are an agent."},
                    {"role": "user", "content": "fix the bug"}
                ]
            })),
            "opening"
        );
    }

    #[test]
    fn after_tool_steps_fingerprint_through_the_wire() {
        // The common in-loop step: the model called <tool>, its result returns
        // as an OpenAI `{role:"tool", tool_call_id, ...}` message (which carries
        // no tool name on the wire). The fingerprint recovers the step from the
        // assistant's tool call, matching `after_<tool>` for every loop tool.
        let after = |tool: &str| {
            serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": "fix the bug"},
                    {"role": "assistant", "content": serde_json::Value::Null,
                     "tool_calls": [
                        {"id": "c1", "type": "function",
                         "function": {"name": tool, "arguments": "{}"}}
                     ]},
                    {"role": "tool", "tool_call_id": "c1", "content": "<result>"}
                ]
            })
        };
        for tool in ["terminal", "patch", "read_file"] {
            assert_eq!(
                fingerprint_of(after(tool)),
                format!("after_{tool}"),
                "the wire parse + fingerprint must label this the after_{tool} step"
            );
        }
    }

    #[test]
    fn trailing_user_turn_is_keyed_by_the_last_model_turn() {
        // A documented divergence from a simpler last-message scheme: a fresh
        // user instruction after a plain model reply is keyed by the model's most
        // recent turn (`midstream`), not by the trailing user message. Neither
        // `midstream` nor a user-followup label is in the demo's converged policy,
        // so this does not affect that workload's routing.
        assert_eq!(
            fingerprint_of(serde_json::json!({
                "model": "m",
                "messages": [
                    {"role": "user", "content": "hi"},
                    {"role": "assistant", "content": "done"},
                    {"role": "user", "content": "now do Y"}
                ]
            })),
            "midstream"
        );
    }
}
