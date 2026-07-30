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

use crate::eval::settlement::{PendingEvalDecision, PendingEvalDecisionStore};
use crate::workflow_state::decision::{PolicyDecisionJsonlRecorder, PolicyDecisionRecord};
use crate::workflow_state::ir::{HarnessId, WorkflowIdentity, WorkflowStateKind};
use crate::workflow_state::online::OnlineWorkflowState;
use crate::workflow_state::session::WorkflowIdentityTracker;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PolicyDecisionReason {
    StaticTable,
    ExplorationTrial,
    ExplorationLocked,
    AdequacyPin,
    ReliabilityCircuitOpen,
    ReliabilityHalfOpenProbe,
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
            Self::ReliabilityCircuitOpen => "reliability_circuit_open",
            Self::ReliabilityHalfOpenProbe => "reliability_half_open_probe",
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
    pub harness_id: HarnessId,
    pub workflow_identity: WorkflowIdentity,
    pub static_tier: Option<String>,
    pub static_model: Option<String>,
    pub selected_tier: Option<String>,
    pub selected_model: Option<String>,
    pub reason: PolicyDecisionReason,
    pub pinned: bool,
    pub request_qualified: bool,
    pub semantic_successes: u32,
    pub semantic_success_threshold: u32,
    pub locked: bool,
    pub trialed: bool,
}

impl PolicyDecision {
    /// Canonical decision records call this value `trace_state`; retain the
    /// established Rust field while exposing the canonical terminology.
    pub fn trace_state(&self) -> &str {
        &self.workflow_state_kind
    }

    /// Canonical decision records call this value `trace_identity`; retain the
    /// established Rust field while exposing the canonical terminology.
    pub fn trace_identity(&self) -> &WorkflowIdentity {
        &self.workflow_identity
    }
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
    /// Whether source-neutral opening requests are eligible for exploration.
    explore_opening: bool,
    /// Reverse index model id → tier name, for mapping a served model back to
    /// its tier at observe time.
    model_to_tier: HashMap<String, String>,
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
            model_to_tier,
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
    pub(crate) fn model_of_tier(&self, tier: &str) -> Option<&str> {
        self.tiers.get(tier).map(String::as_str)
    }

    /// The tier a served model id belongs to (reverse of [`Self::model_of_tier`]).
    /// Used by the adequacy observer to map an outcome back to a tier.
    pub(crate) fn tier_of_model(&self, model: &str) -> Option<&str> {
        if let Some(tier) = self.model_to_tier.get(model) {
            return Some(tier.as_str());
        }
        if model.contains(':') {
            return None;
        }
        let mut matched = None;
        for (tier, route_model) in &self.tiers {
            let Some((_, service_id)) = route_model.split_once(':') else {
                continue;
            };
            if service_id != model {
                continue;
            }
            if matched.is_some() {
                return None;
            }
            matched = Some(tier.as_str());
        }
        matched
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
        OnlineWorkflowState::from_headers(headers, prompt)
            .routing_key()
            .to_string()
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

    pub(crate) fn exploration_allowed_for_prompt(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
    ) -> bool {
        let online = OnlineWorkflowState::from_headers(headers, prompt);
        self.exploration_allowed_for_online(&online)
    }

    fn exploration_allowed_for_online(&self, online: &OnlineWorkflowState) -> bool {
        (self.explore_opening && online.ir.state_kind == WorkflowStateKind::Opening)
            || matches!(
                online.ir.state_kind,
                WorkflowStateKind::ToolFollowup
                    | WorkflowStateKind::Edit
                    | WorkflowStateKind::Test
                    | WorkflowStateKind::Debug
            )
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
/// Build it from [`PolicyTableConfig`] via [`PolicyTableRouter::from_config`]
/// (`None` when no tiers are defined) or [`PolicyTableRouter::new`].
pub struct PolicyTableRouter {
    table: Arc<PolicyTable>,
    decision_recorder: Option<Arc<PolicyDecisionJsonlRecorder>>,
    state_namespace: Option<String>,
    identity_tracker: WorkflowIdentityTracker,
    eval_observer: Option<EvalDecisionObserver>,
}

#[derive(Clone)]
struct EvalDecisionObserver {
    pending: PendingEvalDecisionStore,
    policy: String,
    policy_digest: String,
}

impl PolicyTableRouter {
    /// Build a static router from the `policy_table:` config, or `None` when the
    /// section is inert (no tiers defined) — mirroring
    /// `FusionAliasConfig::from_settings`, so an unconfigured deployment wires no
    /// transform. No adequacy ledger is attached.
    pub fn from_config(config: &PolicyTableConfig) -> Option<Self> {
        PolicyTable::from_config(config).map(|table| Self {
            table,
            decision_recorder: None,
            state_namespace: None,
            identity_tracker: WorkflowIdentityTracker::default(),
            eval_observer: None,
        })
    }

    /// Build a router over an immutable policy table.
    pub fn new(table: Arc<PolicyTable>) -> Self {
        Self {
            table,
            decision_recorder: None,
            state_namespace: None,
            identity_tracker: WorkflowIdentityTracker::default(),
            eval_observer: None,
        }
    }

    pub fn with_decision_recorder(mut self, recorder: PolicyDecisionJsonlRecorder) -> Self {
        self.decision_recorder = Some(Arc::new(recorder));
        self
    }

    /// Namespace diagnostic decision records for a named policy.
    pub(crate) fn with_state_namespace(mut self, namespace: impl Into<String>) -> Self {
        self.state_namespace = Some(namespace.into());
        self
    }

    pub(crate) fn with_shared_decision_recorder(
        mut self,
        recorder: Arc<PolicyDecisionJsonlRecorder>,
    ) -> Self {
        self.decision_recorder = Some(recorder);
        self
    }

    pub(crate) fn with_eval_observer(
        mut self,
        pending: PendingEvalDecisionStore,
        policy: impl Into<String>,
        policy_digest: impl Into<String>,
    ) -> Self {
        self.eval_observer = Some(EvalDecisionObserver {
            pending,
            policy: policy.into(),
            policy_digest: policy_digest.into(),
        });
        self
    }

    fn ledger_key(&self, request_key: &str) -> String {
        self.state_namespace.as_ref().map_or_else(
            || request_key.to_string(),
            |ns| format!("{ns}\0{request_key}"),
        )
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
        self.decision_for_inner(prompt, headers, true)
    }

    fn decision_for_inner(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
        respect_explicit_route: bool,
    ) -> PolicyDecision {
        let online =
            OnlineWorkflowState::from_headers_with_tracker(headers, prompt, &self.identity_tracker);
        let legacy_fingerprint = online.legacy_fingerprint().to_string();
        let request_key = online.routing_key().to_string();
        let mut decision = PolicyDecision {
            key_strategy: PolicyKeyStrategy::AgentTrace,
            request_key,
            legacy_fingerprint,
            workflow_state_kind: online.ir.state_kind.to_string(),
            harness_id: online.ir.harness_id.clone(),
            workflow_identity: online.ir.identity.clone(),
            static_tier: None,
            static_model: None,
            selected_tier: None,
            selected_model: None,
            reason: PolicyDecisionReason::NoMatch,
            pinned: false,
            request_qualified: false,
            semantic_successes: 0,
            semantic_success_threshold: 0,
            locked: false,
            trialed: false,
        };

        if (respect_explicit_route && is_explicitly_routed(&prompt.model))
            || carries_bitrouter_server_tool(prompt)
        {
            return decision;
        }

        let Some(raw_static_tier) = self.table.tier_for_fingerprint(&decision.request_key) else {
            return decision;
        };
        decision.static_tier = Some(raw_static_tier.to_string());
        decision.static_model = self
            .table
            .model_of_tier(raw_static_tier)
            .map(ToString::to_string);
        let (selected_tier, static_clamped) =
            self.table.guardrail_with_status(raw_static_tier, prompt);
        decision.reason = if static_clamped {
            PolicyDecisionReason::ToolGuardrail
        } else {
            PolicyDecisionReason::StaticTable
        };

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

    fn route_prompt(&self, prompt: &mut Prompt, headers: &HeaderMap) -> bool {
        let input_model = prompt.model.clone();
        let decision = self.decision_for(prompt, headers);
        let selected = self.record_decision(input_model, decision, headers);
        let Some(model) = selected else {
            return false;
        };
        if prompt.model == model {
            return false;
        }
        prompt.model = model;
        true
    }

    /// Select a model for a preset that explicitly owns this policy. Unlike the
    /// legacy global transform, a provider-qualified preset base does not opt
    /// out: the preset binding itself is the caller's explicit routing intent.
    pub(crate) fn select_for_bound_policy(
        &self,
        input_model: &str,
        prompt: &Prompt,
        headers: &HeaderMap,
    ) -> Option<String> {
        let decision = self.decision_for_inner(prompt, headers, false);
        self.record_decision(input_model.to_string(), decision, headers)
    }

    fn record_decision(
        &self,
        input_model: String,
        decision: PolicyDecision,
        headers: &HeaderMap,
    ) -> Option<String> {
        let request_id = headers
            .get("x-bitrouter-request-id")
            .and_then(|value| value.to_str().ok())
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let request_id_for_log = request_id.unwrap_or("-");
        tracing::info!(
            request_id = request_id_for_log,
            key_strategy = ?decision.key_strategy,
            request_key = %decision.request_key,
            legacy_fingerprint = %decision.legacy_fingerprint,
            trace_state = %decision.workflow_state_kind,
            trace_parent_session = ?decision.workflow_identity.parent_session_id,
            trace_agent_role = decision.workflow_identity.role.as_str(),
            trace_context_epoch = decision.workflow_identity.context_epoch,
            trace_session_fingerprint = %decision.workflow_identity.fingerprint,
            static_tier = ?decision.static_tier,
            static_model = ?decision.static_model,
            selected_tier = ?decision.selected_tier,
            selected_model = ?decision.selected_model,
            reason = %decision.reason,
            pinned = decision.pinned,
            request_qualified = decision.request_qualified,
            semantic_successes = decision.semantic_successes,
            semantic_success_threshold = decision.semantic_success_threshold,
            locked = decision.locked,
            trialed = decision.trialed,
            "policy routing decision"
        );
        if let (Some(observer), Some(request_id), Some(selected_tier)) = (
            &self.eval_observer,
            request_id,
            decision.selected_tier.as_deref(),
        ) {
            observer.pending.insert(PendingEvalDecision {
                request_id: request_id.to_string(),
                decision_id: format!("{request_id}:{}", observer.policy),
                policy: observer.policy.clone(),
                policy_digest: observer.policy_digest.clone(),
                request_key: decision.request_key.clone(),
                selected_tier: selected_tier.to_string(),
                baseline_tier: decision.static_tier.clone(),
                preset: Some(observer.policy.clone()),
                holdout: false,
            });
        }
        if let Some(recorder) = &self.decision_recorder {
            let record = PolicyDecisionRecord {
                captured_at: None,
                request_id: request_id.map(ToString::to_string),
                input_model,
                key_strategy: key_strategy_name().to_string(),
                request_key: decision.request_key.clone(),
                ledger_key: self
                    .state_namespace
                    .as_ref()
                    .map(|_| self.ledger_key(&decision.request_key)),
                policy: self.eval_observer.as_ref().map(|item| item.policy.clone()),
                policy_digest: self
                    .eval_observer
                    .as_ref()
                    .map(|item| item.policy_digest.clone()),
                preset_variant: self.eval_observer.as_ref().map(|item| item.policy.clone()),
                baseline_tier: decision.static_tier.clone(),
                legacy_fingerprint: decision.legacy_fingerprint.clone(),
                workflow_state: decision.workflow_state_kind.clone(),
                workflow_identity: decision.workflow_identity.clone(),
                static_tier: decision.static_tier.clone(),
                static_model: decision.static_model.clone(),
                selected_tier: decision.selected_tier.clone(),
                selected_model: decision.selected_model.clone(),
                reason: decision.reason.to_string(),
                pinned: decision.pinned,
                request_qualified: decision.request_qualified,
                semantic_successes: decision.semantic_successes,
                semantic_success_threshold: decision.semantic_success_threshold,
                locked: decision.locked,
                trialed: decision.trialed,
            }
            .captured_now();
            if let Err(error) = recorder.record(&record) {
                tracing::warn!(%error, "policy decision recorder failed");
            }
        }
        decision.selected_model
    }
}

fn key_strategy_name() -> &'static str {
    "agent_trace"
}

impl PromptTransform for PolicyTableRouter {
    fn apply(&self, prompt: &mut Prompt) {
        PolicyTableRouter::apply(self, prompt);
    }

    fn apply_with_headers(&self, prompt: &mut Prompt, headers: &HeaderMap) {
        self.route_prompt(prompt, headers);
    }
}

/// Whether `model` already names an explicit upstream route or preset. A
/// `provider:model` id triggers Strategy 1; `@preset` must survive until Stage
/// 0 can resolve its prompt defaults, provider preferences, and named policy.
fn is_explicitly_routed(model: &str) -> bool {
    model.starts_with('@') || model.contains(':')
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
    use crate::workflow_state::decision::PolicyDecisionJsonlRecorder;
    use crate::workflow_state::ir::{AgentRole, HarnessId, ProtocolKind};
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
            key_strategy: PolicyKeyStrategy::AgentTrace,
            tiers: HashMap::from([
                ("cheap".to_string(), "vendor/cheap".to_string()),
                ("flagship".to_string(), "vendor/flagship".to_string()),
            ]),
            fingerprints: HashMap::from([
                (
                    "agent_trace/v1|opening|normal".to_string(),
                    "flagship".to_string(),
                ),
                (
                    "agent_trace/v1|tool_followup|normal".to_string(),
                    "cheap".to_string(),
                ),
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

    fn temp_path(name: &str) -> std::path::PathBuf {
        let unique = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system time after epoch")
            .as_nanos();
        std::env::temp_dir().join(format!(
            "bitrouter-policy-table-{name}-{}-{unique}",
            std::process::id()
        ))
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
    fn equivalent_tool_followups_share_the_trace_projection_route() {
        // Raw tool names do not participate in the agent-trace projection.
        assert_eq!(
            route(
                "inbound",
                vec![user("fix the bug"), assistant_calls("grep")],
                vec![],
            ),
            "vendor/cheap"
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
    fn preset_routes_are_left_for_stage_zero_resolution() {
        assert_eq!(route("@coding", vec![user("hi")], vec![]), "@coding");
        assert_eq!(
            route("@coding:free", vec![user("hi")], vec![]),
            "@coding:free"
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
            fingerprints: HashMap::from([(
                "agent_trace/v1|opening|normal".to_string(),
                "cheap".to_string(),
            )]),
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
    fn route_prompt_writes_policy_decision_jsonl_when_recorder_is_configured() {
        let path = temp_path("decisions.jsonl");
        let table = PolicyTable::from_config(&config()).expect("configured");
        let recorder = PolicyDecisionJsonlRecorder::new(path.clone()).unwrap();
        let r = PolicyTableRouter::new(table)
            .with_state_namespace("coding")
            .with_decision_recorder(recorder);

        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-request-id",
            HeaderValue::from_static("req-001"),
        );
        headers.insert(
            "x-bitrouter-harness",
            HeaderValue::from_static("terminus_2"),
        );
        headers.insert("x-session-id", HeaderValue::from_static("parent-001"));
        headers.insert(
            "x-bitrouter-benchmark-run-id",
            HeaderValue::from_static("short13-run"),
        );
        headers.insert("x-bitrouter-trial-id", HeaderValue::from_static("trial-01"));
        let mut p = prompt("inbound");
        p.system = Some(
            "You are an AI assistant tasked with solving command-line tasks in a Linux environment. Format your response as JSON commands with task_complete.".to_string(),
        );
        p.messages = vec![user("fix the bug"), assistant_calls("read_file")];

        assert!(r.route_prompt(&mut p, &headers));
        assert_eq!(p.model, "vendor/cheap");

        let records =
            crate::workflow_state::decision::PolicyDecisionRecord::load_jsonl(&path).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].request_id.as_deref(), Some("req-001"));
        assert_eq!(records[0].input_model, "inbound");
        assert_eq!(
            records[0].ledger_key.as_deref(),
            Some("coding\0agent_trace/v1|tool_followup|normal")
        );
        assert_eq!(records[0].static_model.as_deref(), Some("vendor/cheap"));
        assert_eq!(records[0].selected_model.as_deref(), Some("vendor/cheap"));
        assert_eq!(records[0].reason, "static_table");
        assert_eq!(records[0].workflow_identity.role, AgentRole::Main);
        assert_eq!(
            records[0].workflow_identity.parent_session_id.as_deref(),
            Some("parent-001")
        );
        assert_eq!(
            records[0].workflow_identity.benchmark_run_id.as_deref(),
            Some("short13-run")
        );
        assert_eq!(
            records[0].workflow_identity.trial_id.as_deref(),
            Some("trial-01")
        );
        assert!(
            records[0]
                .workflow_identity
                .fingerprint
                .starts_with("sha256:")
        );

        let _ = std::fs::remove_file(path);
    }

    #[test]
    fn routed_request_is_correlated_for_generic_eval_settlement() {
        let table = PolicyTable::from_config(&config()).expect("configured");
        let pending = crate::eval::settlement::PendingEvalDecisionStore::default();
        let router = PolicyTableRouter::new(table).with_eval_observer(
            pending.clone(),
            "auto:cost",
            "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef",
        );
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-bitrouter-request-id",
            HeaderValue::from_static("request-1"),
        );
        let mut routed = prompt("inbound");
        routed.messages = vec![user("fix the bug")];

        assert!(router.route_prompt(&mut routed, &headers));

        let decision = pending.get("request-1").expect("pending eval decision");
        assert_eq!(decision.policy, "auto:cost");
        assert_eq!(decision.selected_tier, "flagship");
        assert_eq!(decision.request_key, "agent_trace/v1|opening|normal");
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
            fingerprints: HashMap::from([(
                "agent_trace/v1|opening|normal".to_string(),
                "flagship".to_string(),
            )]),
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
            fingerprints: HashMap::from([(
                "agent_trace/v1|tool_followup|normal".to_string(),
                "cheap".to_string(),
            )]),
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

    // ---- canonical agent-trace routing ----

    /// A read step prompt — fingerprints to `after_read_file` (→ cheap statically).
    fn read_step() -> Vec<Message> {
        vec![user("fix the bug"), assistant_calls("read_file")]
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
        cfg.key_strategy = PolicyKeyStrategy::AgentTrace;
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
            fingerprints: HashMap::from([(
                "agent_trace/v1|opening|normal".to_string(),
                "cheap".to_string(),
            )]),
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

    #[test]
    fn policy_table_decision_never_exposes_learned_override_reasons() {
        let router = PolicyTableRouter::from_config(&config()).expect("configured");
        let mut prompt = prompt("inbound");
        prompt.messages = read_step();

        let decision = router.decision_for(&prompt, &HeaderMap::new());

        assert_eq!(decision.reason, PolicyDecisionReason::StaticTable);
        assert!(!decision.pinned);
        assert!(!decision.request_qualified);
        assert_eq!(decision.semantic_successes, 0);
        assert_eq!(decision.semantic_success_threshold, 0);
        assert!(!decision.locked);
        assert!(!decision.trialed);
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
