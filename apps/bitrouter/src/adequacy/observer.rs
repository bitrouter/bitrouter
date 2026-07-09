//! The adequacy observe hook — writes the ledger from each request's outcome.
//!
//! Registered as an [`ObserveHook`], it runs after every request. On
//! `on_request_end` it recomputes the request fingerprint, maps the served model
//! back to its tier, and — only for a *downgrade* (a tier other than the
//! escalation tier) — records whether the outcome was a hard failure. Repeated
//! failures on a downgrade pin the fingerprint in the [`AdequacyLedger`], so
//! future requests with that fingerprint escalate.

use std::sync::Arc;

use async_trait::async_trait;

use bitrouter_sdk::BitrouterError;
use bitrouter_sdk::language_model::types::StreamPart;
use bitrouter_sdk::language_model::{
    ObserveHook, Phase, PipelineContext, RequestOutcome, StreamContext,
};

use crate::adequacy::{AdequacyLedger, InadequacyCause, Outcome};
use crate::policy_table_router::PolicyTable;

/// Feeds the [`AdequacyLedger`] from request outcomes against the shared
/// [`PolicyTable`].
pub struct AdequacyObserveHook {
    table: Arc<PolicyTable>,
    ledger: Arc<AdequacyLedger>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum ObservationDecision {
    Record {
        fingerprint: String,
        outcome: Outcome,
        served_tier: String,
        static_tier: Option<String>,
        escalation_tier: Option<String>,
    },
    Skip {
        reason: &'static str,
        fingerprint: String,
        served_tier: Option<String>,
        static_tier: Option<String>,
        escalation_tier: Option<String>,
        exploration_allowed: bool,
        trialed: bool,
    },
}

impl AdequacyObserveHook {
    /// Build the hook over the shared policy table and ledger.
    pub fn new(table: Arc<PolicyTable>, ledger: Arc<AdequacyLedger>) -> Self {
        Self { table, ledger }
    }

    fn observation_decision(
        &self,
        ctx: &PipelineContext,
        outcome: &RequestOutcome,
    ) -> ObservationDecision {
        let client_disconnected = matches!(outcome, RequestOutcome::ClientDisconnected);
        let cause = match outcome {
            RequestOutcome::Failed(error) => classify_failure(error),
            // A disconnect is not proof that a cheap response was adequate, but
            // for an exploration candidate left on the escalation tier it can
            // still advance the deterministic trial cadence below.
            RequestOutcome::ClientDisconnected => InadequacyCause::None,
            // A completed request got a response: the route held.
            RequestOutcome::Completed => InadequacyCause::None,
        };
        let fingerprint = self.table.request_key(ctx.prompt(), ctx.headers());
        let served_tier = self.table.tier_of_model(ctx.model()).map(str::to_string);
        let static_tier = self
            .table
            .static_tier_with_headers(ctx.prompt(), ctx.headers())
            .map(str::to_string);
        let escalation_tier = self.table.escalation_tier().map(str::to_string);
        let Some(served_tier) = served_tier else {
            return ObservationDecision::Skip {
                reason: "served_model_not_in_policy_tiers",
                fingerprint,
                served_tier: None,
                static_tier,
                escalation_tier,
                exploration_allowed: false,
                trialed: false,
            };
        };

        // A genuine *static* (operator-configured) downgrade: the served tier is
        // exactly the static decision, and it is a downgrade (not the escalation
        // tier). This guards against a caller's explicit route / coincidental
        // model match being mistaken for one.
        if static_tier.as_deref() == Some(served_tier.as_str())
            && Some(served_tier.as_str()) != escalation_tier.as_deref()
        {
            if client_disconnected {
                return ObservationDecision::Skip {
                    reason: "static_downgrade_client_disconnected",
                    fingerprint,
                    served_tier: Some(served_tier),
                    static_tier,
                    escalation_tier,
                    exploration_allowed: false,
                    trialed: false,
                };
            }
            return ObservationDecision::Record {
                fingerprint,
                outcome: Outcome::StaticDowngrade { cause },
                served_tier,
                static_tier,
                escalation_tier,
            };
        }

        if !self.table.exploration_enabled() {
            return ObservationDecision::Skip {
                reason: "exploration_disabled",
                fingerprint,
                served_tier: Some(served_tier),
                static_tier,
                escalation_tier,
                exploration_allowed: false,
                trialed: false,
            };
        }
        let Some(escalation_tier_value) = escalation_tier.as_deref() else {
            return ObservationDecision::Skip {
                reason: "missing_escalation_tier",
                fingerprint,
                served_tier: Some(served_tier),
                static_tier,
                escalation_tier,
                exploration_allowed: false,
                trialed: false,
            };
        };
        if static_tier.as_deref() != Some(escalation_tier_value) {
            return ObservationDecision::Skip {
                reason: "static_tier_not_escalation_tier",
                fingerprint,
                served_tier: Some(served_tier),
                static_tier,
                escalation_tier,
                exploration_allowed: false,
                trialed: false,
            };
        }
        let exploration_allowed = self
            .table
            .exploration_allowed_for_prompt(ctx.prompt(), ctx.headers());
        if !exploration_allowed {
            return ObservationDecision::Skip {
                reason: "exploration_not_allowed_for_prompt",
                fingerprint,
                served_tier: Some(served_tier),
                static_tier,
                escalation_tier,
                exploration_allowed,
                trialed: false,
            };
        }

        let trialed = self.table.explore_tier() == Some(served_tier.as_str());
        let served_escalation = Some(served_tier.as_str()) == escalation_tier.as_deref();
        // Count only a trial (served the explore tier) or a cadence-advance
        // (served the escalation tier). A candidate served on some third tier
        // — e.g. a tool request whose explore-tier trial the guardrail clamped
        // up to the tool-use tier — is intentionally not counted: a real trial
        // there would be clamped too, so exploration is correctly inert for it.
        if !trialed && !served_escalation {
            return ObservationDecision::Skip {
                reason: "served_tier_not_counted_for_exploration",
                fingerprint,
                served_tier: Some(served_tier),
                static_tier,
                escalation_tier,
                exploration_allowed,
                trialed,
            };
        }
        if trialed && client_disconnected {
            return ObservationDecision::Skip {
                reason: "exploration_trial_client_disconnected",
                fingerprint,
                served_tier: Some(served_tier),
                static_tier,
                escalation_tier,
                exploration_allowed,
                trialed,
            };
        }

        ObservationDecision::Record {
            fingerprint,
            outcome: Outcome::Exploration { trialed, cause },
            served_tier,
            static_tier,
            escalation_tier,
        }
    }
}

#[async_trait]
impl ObserveHook for AdequacyObserveHook {
    // Per-phase and per-stream-part observation are unused — all the work is at
    // request end, where the served model and final outcome are both known.
    async fn after_phase(&self, _phase: Phase, _ctx: &PipelineContext) {}

    async fn on_stream_part(&self, _ctx: &StreamContext, _part: &StreamPart) {}

    async fn on_request_end(&self, ctx: &PipelineContext, outcome: &RequestOutcome) {
        match self.observation_decision(ctx, outcome) {
            ObservationDecision::Record {
                fingerprint,
                outcome,
                served_tier,
                static_tier,
                escalation_tier,
            } => {
                tracing::debug!(
                    request_id = %ctx.request_id(),
                    model = %ctx.model(),
                    fingerprint = %fingerprint,
                    served_tier = %served_tier,
                    static_tier = ?static_tier,
                    escalation_tier = ?escalation_tier,
                    observation = ?outcome,
                    "adequacy observation recorded"
                );
                self.ledger.observe(&fingerprint, outcome).await;
            }
            ObservationDecision::Skip {
                reason,
                fingerprint,
                served_tier,
                static_tier,
                escalation_tier,
                exploration_allowed,
                trialed,
            } => {
                tracing::debug!(
                    request_id = %ctx.request_id(),
                    model = %ctx.model(),
                    fingerprint = %fingerprint,
                    served_tier = ?served_tier,
                    static_tier = ?static_tier,
                    escalation_tier = ?escalation_tier,
                    exploration_allowed,
                    trialed,
                    reason,
                    "adequacy observation skipped"
                );
            }
        }
    }
}

fn classify_failure(error: &BitrouterError) -> InadequacyCause {
    match error {
        BitrouterError::Upstream { status, .. } => match *status {
            408 | 429 | 500..=599 => InadequacyCause::ProviderTransient,
            401 | 403 => InadequacyCause::Auth,
            _ => InadequacyCause::ProviderPermanent,
        },
        BitrouterError::UpstreamTimeout
        | BitrouterError::RateLimited { .. }
        | BitrouterError::Internal(_) => InadequacyCause::ProviderTransient,
        BitrouterError::UpstreamAuth { .. }
        | BitrouterError::Unauthorized(_)
        | BitrouterError::Forbidden(_)
        | BitrouterError::PaymentRequired(_) => InadequacyCause::Auth,
        BitrouterError::BadRequest { .. } => InadequacyCause::Protocol,
        BitrouterError::NotFound(_) => InadequacyCause::Client,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    use bitrouter_sdk::BitrouterError;
    use bitrouter_sdk::HeaderMap;
    use bitrouter_sdk::caller::CallerContext;
    use bitrouter_sdk::config::{PolicyKeyStrategy, PolicyTableConfig};
    use bitrouter_sdk::language_model::types::{
        Content, GenerationParams, Message, PipelineRequest, Prompt, ProviderMetadata, Role, Tool,
    };
    use http::HeaderValue;

    use crate::workflow_state::ir::{HarnessId, ProtocolKind};
    use crate::workflow_state::online::OnlineWorkflowState;

    // A table: `opening` → capable (= the escalation tier, via default_tier),
    // `after_read_file` → cheap (a downgrade).
    fn table() -> Arc<PolicyTable> {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([
                ("cheap".to_string(), "vendor/cheap".to_string()),
                ("capable".to_string(), "vendor/capable".to_string()),
            ]),
            fingerprints: HashMap::from([
                ("opening".to_string(), "capable".to_string()),
                ("after_read_file".to_string(), "cheap".to_string()),
            ]),
            default_tier: Some("capable".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        PolicyTable::from_config(&cfg).expect("configured")
    }

    fn workflow_table(workflow_key: String) -> Arc<PolicyTable> {
        let cfg = PolicyTableConfig {
            key_strategy: PolicyKeyStrategy::WorkflowState,
            tiers: HashMap::from([
                ("cheap".to_string(), "vendor/cheap".to_string()),
                ("capable".to_string(), "vendor/capable".to_string()),
            ]),
            fingerprints: HashMap::from([(workflow_key, "cheap".to_string())]),
            default_tier: Some("capable".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: Default::default(),
        };
        PolicyTable::from_config(&cfg).expect("configured")
    }

    // The same table with exploration on: `opening` (static = capable = the
    // escalation tier) is a candidate trialed toward `cheap`.
    fn explore_table() -> Arc<PolicyTable> {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([
                ("cheap".to_string(), "vendor/cheap".to_string()),
                ("capable".to_string(), "vendor/capable".to_string()),
            ]),
            fingerprints: HashMap::from([("opening".to_string(), "capable".to_string())]),
            default_tier: Some("capable".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: bitrouter_sdk::config::AdequacyConfig {
                enabled: true,
                explore_enabled: true,
                explore_tier: Some("cheap".to_string()),
                explore_opening: true,
                ..Default::default()
            },
        };
        PolicyTable::from_config(&cfg).expect("configured")
    }

    fn explicit_route_explore_table() -> Arc<PolicyTable> {
        let cfg = PolicyTableConfig {
            key_strategy: Default::default(),
            tiers: HashMap::from([
                (
                    "cheap".to_string(),
                    "bitrouter:moonshotai/kimi-k2.7-code".to_string(),
                ),
                ("capable".to_string(), "openai-codex:gpt-5.5".to_string()),
            ]),
            fingerprints: HashMap::from([("opening".to_string(), "capable".to_string())]),
            default_tier: Some("capable".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: Vec::new(),
            adequacy: bitrouter_sdk::config::AdequacyConfig {
                enabled: true,
                explore_enabled: true,
                explore_tier: Some("cheap".to_string()),
                ..Default::default()
            },
        };
        PolicyTable::from_config(&cfg).expect("configured")
    }

    fn explicit_route_workflow_explore_table() -> Arc<PolicyTable> {
        let cfg = PolicyTableConfig {
            key_strategy: PolicyKeyStrategy::WorkflowState,
            tiers: HashMap::from([
                (
                    "cheap".to_string(),
                    "bitrouter:moonshotai/kimi-k2.7-code".to_string(),
                ),
                ("capable".to_string(), "openai-codex:gpt-5.5".to_string()),
            ]),
            fingerprints: HashMap::new(),
            default_tier: Some("capable".to_string()),
            tool_use_tier: None,
            tool_safe_tiers: vec!["cheap".to_string(), "capable".to_string()],
            adequacy: bitrouter_sdk::config::AdequacyConfig {
                enabled: true,
                escalation_tier: Some("capable".to_string()),
                explore_enabled: true,
                explore_tier: Some("cheap".to_string()),
                explore_opening: false,
                ..Default::default()
            },
        };
        PolicyTable::from_config(&cfg).expect("configured")
    }

    fn user(text: &str) -> Message {
        Message::text(Role::User, text)
    }

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

    fn prompt(messages: Vec<Message>) -> Prompt {
        Prompt {
            model: String::new(),
            system: None,
            system_provider_metadata: ProviderMetadata::new(),
            messages,
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn prompt_with_tools(messages: Vec<Message>, tools: Vec<Tool>) -> Prompt {
        Prompt {
            tools,
            ..prompt(messages)
        }
    }

    /// A context for a request the pipeline served on `served_model`.
    fn ctx(served_model: &str, messages: Vec<Message>) -> PipelineContext {
        let request = PipelineRequest::new(
            served_model.to_string(),
            CallerContext::new("k", "u"),
            prompt(messages),
        );
        PipelineContext::new(request)
    }

    fn ctx_with_headers(
        served_model: &str,
        messages: Vec<Message>,
        headers: HeaderMap,
    ) -> PipelineContext {
        let mut request = PipelineRequest::new(
            served_model.to_string(),
            CallerContext::new("k", "u"),
            prompt(messages),
        );
        request.headers = headers;
        PipelineContext::new(request)
    }

    fn ctx_with_headers_and_tools(
        served_model: &str,
        messages: Vec<Message>,
        tools: Vec<Tool>,
        headers: HeaderMap,
    ) -> PipelineContext {
        let mut request = PipelineRequest::new(
            served_model.to_string(),
            CallerContext::new("k", "u"),
            prompt_with_tools(messages, tools),
        );
        request.headers = headers;
        PipelineContext::new(request)
    }

    fn claude_headers() -> HeaderMap {
        let mut headers = HeaderMap::new();
        headers.insert(
            "anthropic-beta",
            HeaderValue::from_static("claude-code-20250219,tools-2024-05-16"),
        );
        headers
    }

    fn failed() -> RequestOutcome {
        RequestOutcome::Failed(BitrouterError::Upstream {
            status: 400,
            message: "provider rejected the request".to_string(),
        })
    }

    fn transient_failed() -> RequestOutcome {
        RequestOutcome::Failed(BitrouterError::Upstream {
            status: 502,
            message: "provider temporarily unavailable".to_string(),
        })
    }

    fn read_step() -> Vec<Message> {
        vec![user("fix the bug"), assistant_calls("read_file")]
    }

    fn bash_step() -> Vec<Message> {
        vec![user("run command"), assistant_calls("bash")]
    }

    fn exec_command_medium_step() -> Vec<Message> {
        vec![
            user(&format!("run command {}", "x".repeat(12_000))),
            assistant_calls("exec_command"),
        ]
    }

    fn exec_command_tool() -> Tool {
        Tool::Function {
            name: "exec_command".to_string(),
            description: None,
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "cmd": { "type": "string" }
                },
                "required": ["cmd"]
            }),
            strict: None,
            provider_metadata: ProviderMetadata::new(),
        }
    }

    #[tokio::test]
    async fn a_failed_downgrade_pins_the_fingerprint() {
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        // `after_read_file` → cheap is the static downgrade, and the request was
        // served by the cheap model — a genuine downgrade. A hard failure pins it.
        hook.on_request_end(&ctx("vendor/cheap", read_step()), &failed())
            .await;
        assert!(ledger.is_pinned("after_read_file"));
    }

    #[tokio::test]
    async fn a_transient_provider_failure_does_not_pin_on_first_observation() {
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(&ctx("vendor/cheap", read_step()), &transient_failed())
            .await;
        assert!(!ledger.is_pinned("after_read_file"));
    }

    #[tokio::test]
    async fn workflow_key_strategy_observes_pins_by_ir_key() {
        let messages = read_step();
        let prompt = prompt(messages.clone());
        let headers = claude_headers();
        let workflow_key = OnlineWorkflowState::from_prompt(
            &headers,
            &prompt,
            Some(HarnessId::ClaudeCode),
            ProtocolKind::Messages,
        )
        .routing_key()
        .to_string();
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(workflow_table(workflow_key.clone()), ledger.clone());

        hook.on_request_end(
            &ctx_with_headers("vendor/cheap", messages, headers),
            &failed(),
        )
        .await;

        assert!(ledger.is_pinned(&workflow_key));
        assert!(!ledger.is_pinned("after_read_file"));
    }

    #[tokio::test]
    async fn a_failure_not_matching_the_static_downgrade_is_ignored() {
        // The over-attribution guard: an `opening` request served by the cheap
        // model (e.g. the caller routed there) is NOT the static decision
        // (opening → capable), so its failure must not pin anything.
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(&ctx("vendor/cheap", vec![user("start")]), &failed())
            .await;
        assert!(!ledger.is_pinned("opening"));
        assert!(!ledger.is_pinned("after_read_file"));
    }

    #[tokio::test]
    async fn a_failure_on_the_escalation_tier_is_ignored() {
        // `opening` → capable, which is the escalation tier; a failure there is
        // not a downgrade to pin.
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(&ctx("vendor/capable", vec![user("start")]), &failed())
            .await;
        assert!(!ledger.is_pinned("opening"));
    }

    #[tokio::test]
    async fn a_completed_downgrade_does_not_pin() {
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(
            &ctx("vendor/cheap", read_step()),
            &RequestOutcome::Completed,
        )
        .await;
        assert!(!ledger.is_pinned("after_read_file"));
    }

    #[tokio::test]
    async fn a_client_disconnect_is_ignored() {
        let ledger = Arc::new(AdequacyLedger::in_memory(1, 0));
        let hook = AdequacyObserveHook::new(table(), ledger.clone());
        hook.on_request_end(
            &ctx("vendor/cheap", read_step()),
            &RequestOutcome::ClientDisconnected,
        )
        .await;
        assert!(!ledger.is_pinned("after_read_file"));
    }

    // ---- exploration classification ----

    #[tokio::test]
    async fn a_failed_trial_pins_the_candidate() {
        // `opening` is an exploration candidate (static = capable = escalation).
        // A request served by the cheap (explore) tier is a trial; failing it pins.
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 1, 1));
        let hook = AdequacyObserveHook::new(explore_table(), ledger.clone());
        hook.on_request_end(&ctx("vendor/cheap", vec![user("start")]), &failed())
            .await;
        assert!(ledger.is_pinned("opening"));
    }

    #[tokio::test]
    async fn an_adequate_trial_advances_toward_a_lock() {
        // explore_threshold 1 → one adequate trial locks the downgrade.
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 1, 1));
        let hook = AdequacyObserveHook::new(explore_table(), ledger.clone());
        hook.on_request_end(
            &ctx("vendor/cheap", vec![user("start")]),
            &RequestOutcome::Completed,
        )
        .await;
        assert!(ledger.is_locked("opening"), "an adequate trial locks it");
    }

    #[tokio::test]
    async fn a_candidate_on_the_escalation_tier_advances_the_cadence() {
        // A candidate served by the escalation tier is not a trial — it only
        // advances the trial cadence (interval 2 → due after two such requests).
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 2));
        let hook = AdequacyObserveHook::new(explore_table(), ledger.clone());
        assert!(!ledger.should_trial("opening"));
        for _ in 0..2 {
            hook.on_request_end(
                &ctx("vendor/capable", vec![user("start")]),
                &RequestOutcome::Completed,
            )
            .await;
        }
        assert!(ledger.should_trial("opening"), "the cadence advanced");
    }

    #[tokio::test]
    async fn explicit_provider_route_service_id_advances_exploration_cadence() {
        // Real providers report the served service id (`gpt-5.5`) in the
        // pipeline/settlement context, while the policy tier stores the explicit
        // route (`openai-codex:gpt-5.5`). The observer must still map the
        // completed capable request back to the capable tier; otherwise
        // exploration never learns from subscription-backed models.
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 1, 1));
        let hook = AdequacyObserveHook::new(explicit_route_explore_table(), ledger.clone());

        hook.on_request_end(&ctx("gpt-5.5", bash_step()), &RequestOutcome::Completed)
            .await;

        assert!(
            ledger.should_trial("after_bash"),
            "served service id must advance the explicit route tier's cadence"
        );
    }

    #[tokio::test]
    async fn harbor_shaped_codex_tool_followup_is_an_exploration_observation() {
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 2, 1));
        let hook = AdequacyObserveHook::new(explicit_route_workflow_explore_table(), ledger);
        let mut headers = HeaderMap::new();
        headers.insert("x-bitrouter-harness", HeaderValue::from_static("codex"));
        headers.insert(
            "x-bitrouter-protocol",
            HeaderValue::from_static("responses"),
        );

        let ctx = ctx_with_headers_and_tools(
            "gpt-5.5",
            exec_command_medium_step(),
            vec![exec_command_tool(), exec_command_tool()],
            headers,
        );
        let decision = hook.observation_decision(&ctx, &RequestOutcome::Completed);

        assert!(
            matches!(
                &decision,
                ObservationDecision::Record {
                    fingerprint,
                    outcome: Outcome::Exploration {
                        trialed: false,
                        cause: InadequacyCause::None,
                    },
                    ..
                } if fingerprint == "codex|responses|tool_followup|-|-|exec_command|high|medium|none|high|low|medium|low|medium|medium|requires_structured_tools"
            ),
            "Harbor-shaped Codex tool followups must advance exploration cadence, got {decision:?}"
        );
    }

    #[tokio::test]
    async fn opening_candidate_does_not_advance_when_opening_exploration_is_disabled() {
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 1, 1));
        let hook = AdequacyObserveHook::new(explicit_route_explore_table(), ledger.clone());

        hook.on_request_end(
            &ctx("gpt-5.5", vec![user("start")]),
            &RequestOutcome::Completed,
        )
        .await;

        assert!(
            !ledger.should_trial("opening"),
            "opening must not accumulate exploration cadence unless explicitly enabled"
        );
    }

    #[tokio::test]
    async fn client_disconnect_on_escalation_tier_advances_exploration_cadence() {
        // Codex streaming clients may close the response after consuming the
        // useful content. That is not proof that a cheap trial was adequate, but
        // a capable-tier non-trial can still advance the deterministic trial
        // cadence; otherwise streaming agents never explore.
        let ledger = Arc::new(AdequacyLedger::in_memory_explore(1, 0, 1, 1));
        let hook = AdequacyObserveHook::new(explicit_route_explore_table(), ledger.clone());

        hook.on_request_end(
            &ctx("gpt-5.5", bash_step()),
            &RequestOutcome::ClientDisconnected,
        )
        .await;

        assert!(
            ledger.should_trial("after_bash"),
            "capable stream disconnect should still advance exploration cadence"
        );
    }
}
