use super::*;
use crate::gateway::routing::assessment::Judgment;
use anyhow::{Context, Result};
use bitrouter_ai::types::{
    Content, GenerationParams, Message, ProviderMetadata, Role, ToolResultOutput,
};
use http::HeaderValue;
use std::collections::BTreeMap;

#[test]
fn exact_class_unknown_family_and_default_are_ordered() {
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    let prompt = prompt("bitrouter/auto");
    let mut spec = table();
    spec.routes
        .insert(assessment.route_class.to_string(), "flagship".into());
    let router = PolicyTableRouter::new(Arc::new(spec));
    let decision =
        router.decision_for_routing(&prompt, &HeaderMap::new(), Some(&assessment), false);
    assert_eq!(decision.selected_model.as_deref(), Some("vendor/flagship"));
    assert_eq!(decision.request_key, "code:debugging|implement|normal");
    assert!(router.replace_table(Arc::new(table())));
    let decision =
        router.decision_for_routing(&prompt, &HeaderMap::new(), Some(&assessment), false);
    assert_eq!(decision.selected_model.as_deref(), Some("vendor/cheap"));
    assert_eq!(decision.request_key, "unknown|implement|normal");
    assert_eq!(decision.route_projection, "code:debugging|implement|normal");
    assert_eq!(
        router.eval_baseline_tier(&decision).as_deref(),
        Some("cheap")
    );
    let mut spec = table();
    spec.routes.clear();
    assert!(router.replace_table(Arc::new(spec)));
    let decision =
        router.decision_for_routing(&prompt, &HeaderMap::new(), Some(&assessment), false);
    assert_eq!(decision.selected_model.as_deref(), Some("vendor/flagship"));
    assert_eq!(decision.request_key, "code:debugging|implement|normal");
    let absent = router.decision_for_routing(&prompt, &HeaderMap::new(), None, false);
    assert_eq!(absent.route_projection, "unknown|unknown|guarded");
    assert_eq!(absent.selected_model.as_deref(), Some("vendor/flagship"));
    assert!(router.replace_table(PolicyTable::inert()));
    let absent = router.decision_for_routing(&prompt, &HeaderMap::new(), None, false);
    assert_eq!(absent.reason, PolicyDecisionReason::NoMatch);
    assert!(absent.selected_model.is_none());
}

#[test]
fn headers_and_tool_names_cannot_override_the_semantic_class() {
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    let router = router();
    for (header, value) in [
        ("user-agent", "coding-client"),
        ("x-client-session", "session"),
        ("anthropic-beta", "tools"),
    ] {
        let mut headers = HeaderMap::new();
        headers.insert(header, HeaderValue::from_static(value));
        let mut prompt = prompt("vendor:explicit");
        prompt.messages = vec![assistant_calls("unknown_tool")];
        let selected = router.decision_for_bound_policy(&prompt, &headers, &assessment);
        assert_eq!(selected.selected_model.as_deref(), Some("vendor/cheap"));
        assert_eq!(selected.route_projection, "code:debugging|implement|normal");
    }
}

#[test]
fn guardrail_preserves_the_static_action_measurement() -> Result<()> {
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    let router = router();
    let mut prompt = prompt("bitrouter/auto");
    prompt.tools.push(a_tool());
    let guarded = router.decision_for_bound_policy(&prompt, &HeaderMap::new(), &assessment);
    assert_eq!(guarded.static_tier.as_deref(), Some("cheap"));
    assert_eq!(guarded.selected_tier.as_deref(), Some("flagship"));
    let measurement = guarded.route_measurement.context("measurement")?;
    assert_eq!(measurement.logging_action_tier, "cheap");
    assert_eq!(measurement.logging_action_probability_ppm, 1_000_000);
    assert_eq!(measurement.candidates.len(), 2);
    for candidate in measurement.candidates {
        assert_eq!(
            candidate.logging_probability_ppm,
            if candidate.tier == "cheap" {
                1_000_000
            } else {
                0
            }
        );
    }
    let unclamped = router.candidate_for_guarded_policy(&prompt, &HeaderMap::new(), &assessment);
    assert_eq!(unclamped.selected_tier.as_deref(), Some("cheap"));
    Ok(())
}

#[test]
fn continuation_and_recording_use_the_original_table_after_reload() -> Result<()> {
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    let router = router();
    let prompt = prompt("bitrouter/auto");
    let mut decision = router.decision_for_bound_policy(&prompt, &HeaderMap::new(), &assessment);
    assert!(router.replace_table(PolicyTable::inert()));
    router.apply_continuation_adjustment(
        &mut decision,
        &ContinuationAdjustment::Pin {
            effective_model: "vendor/flagship".into(),
            effective_effort: None,
            effort_authoritative: true,
        },
    )?;
    assert_eq!(
        decision.continuation_proposed_model.as_deref(),
        Some("vendor/cheap")
    );
    let target = router
        .record_decision(prompt.model, None, decision, &HeaderMap::new(), None, None)
        .context("target")?;
    assert_eq!(target.model(), "vendor/flagship");
    assert!(
        router
            .decision_for_routing(
                &self::prompt("bitrouter/auto"),
                &HeaderMap::new(),
                None,
                false
            )
            .selected_model
            .is_none()
    );
    Ok(())
}

#[test]
fn model_and_effort_targets_preserve_caller_and_continuation_semantics() -> Result<()> {
    for compound in [false, true] {
        let mut spec = table();
        spec.tiers.insert(
            "flagship".into(),
            Target {
                model: "vendor/same".into(),
                effort: compound.then_some(ReasoningEffort::High),
            },
        );
        let router = PolicyTableRouter::new(Arc::new(spec));
        let assessment = semantic_fixture(TaskFamily::Unknown, NextStep::Unknown);
        let mut prompt = prompt("vendor/same");
        prompt.params.reasoning_effort = Some(ReasoningEffort::Low);
        let decision = router.decision_for_bound_policy(&prompt, &HeaderMap::new(), &assessment);
        let expected = if compound {
            ReasoningEffort::High
        } else {
            ReasoningEffort::Low
        };
        assert_eq!(decision.static_effort, Some(expected));
        assert_eq!(decision.selected_effort, Some(expected));
        assert_eq!(router.apply(&mut prompt, &assessment), compound);
        assert_eq!(prompt.params.reasoning_effort, Some(expected));
        assert_eq!(prompt.model, "vendor/same");
    }
    for authoritative in [false, true] {
        let router = router();
        let mut prompt = prompt("bitrouter/auto");
        prompt.params.reasoning_effort = Some(ReasoningEffort::Low);
        let mut decision = router.decision_for_routing(&prompt, &HeaderMap::new(), None, false);
        router.apply_continuation_adjustment(
            &mut decision,
            &ContinuationAdjustment::Pin {
                effective_model: "vendor/flagship".into(),
                effective_effort: Some(ReasoningEffort::High),
                effort_authoritative: authoritative,
            },
        )?;
        let expected = if authoritative {
            ReasoningEffort::High
        } else {
            ReasoningEffort::Low
        };
        assert_eq!(decision.selected_effort, Some(expected));
        let selected = router
            .record_decision(
                prompt.model,
                prompt.params.reasoning_effort,
                decision,
                &HeaderMap::new(),
                None,
                None,
            )
            .context("selected target")?;
        assert_eq!(selected.effort(), Some(expected));
    }
    Ok(())
}

#[test]
fn eval_baseline_and_append_records_keep_opaque_identity_and_actual_proposals() -> Result<()> {
    let directory = tempfile::tempdir()?;
    let path = directory.path().join("decisions.jsonl");
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    let mut spec = table();
    spec.routes
        .insert(assessment.route_class.to_string(), "cheap".into());
    let pending = PendingEvalDecisionStore::default();
    let router = PolicyTableRouter::new(Arc::new(spec))
        .with_decision_recorder(PolicyDecisionJsonlRecorder::new(path.clone())?)
        .with_eval_observer(
            pending.clone(),
            "auto",
            format!("sha256:{}", "c".repeat(64)),
            HashMap::from([(assessment.route_class.to_string(), "flagship".into())]),
            Some("cheap".into()),
        );
    for (id, adjustment) in [
        ("opaque-one", ContinuationAdjustment::Detach),
        ("opaque-two", ContinuationAdjustment::RejectLegacy),
    ] {
        let prompt = prompt("bitrouter/auto");
        let mut decision =
            router.decision_for_bound_policy(&prompt, &HeaderMap::new(), &assessment);
        router.apply_continuation_adjustment(&mut decision, &adjustment)?;
        let invocation = EvalInvocation::new("owner");
        let (target, captured) = router.record_bound_policy_decision(
            (id, "private-ingress-id"),
            &invocation,
            prompt.model,
            None,
            decision,
            &HeaderMap::new(),
        );
        assert_eq!(target.as_ref().map(Target::model), Some("vendor/cheap"));
        let stored = pending
            .get(&invocation, "owner")
            .context("pending decision")?;
        assert_eq!(captured.as_ref(), Some(&stored));
        assert!(pending.get(&invocation, "other-owner").is_none());
        assert_eq!(stored.baseline_tier.as_deref(), Some("flagship"));
        assert_eq!(stored.continuation_proposed_tier.as_deref(), Some("cheap"));
    }
    let bytes = std::fs::read_to_string(path)?;
    assert!(!bytes.contains("private-ingress-id"));
    let records: Vec<PolicyDecisionRecord> = bytes
        .lines()
        .map(serde_json::from_str)
        .collect::<std::result::Result<_, _>>()?;
    assert_eq!(records.len(), 2);
    for (record, (id, adjustment)) in records
        .iter()
        .zip([("opaque-one", "detach"), ("opaque-two", "reject_legacy")])
    {
        assert_eq!(record.request_id.as_deref(), Some(id));
        assert_eq!(
            record.ingress_request_id_sha256,
            Some(
                crate::gateway::routing::workflow::decision::ingress_request_id_sha256(
                    "private-ingress-id"
                )
            )
        );
        assert_eq!(record.baseline_tier.as_deref(), Some("flagship"));
        assert_eq!(record.selected_model.as_deref(), Some("vendor/cheap"));
        assert_eq!(
            record.continuation_proposed_model.as_deref(),
            Some("vendor/cheap")
        );
        assert_eq!(record.continuation_adjustment.as_deref(), Some(adjustment));
        assert!(record.captured_at.is_some());
    }
    Ok(())
}

fn semantic_fixture(task_family: TaskFamily, next_step_role: NextStep) -> Assessment {
    let labels = [
        (crate::gateway::routing::assessment::TASK, task_family.key()),
        (
            crate::gateway::routing::assessment::ROLE,
            next_step_role.key(),
        ),
        (crate::gateway::routing::assessment::PROGRESS, "progressing"),
    ];
    Assessment {
        model: "semantic-fixture-v1".into(),
        requested_model: "semantic-fixture".into(),
        confidence_threshold: 0.85,
        classifier_digest: format!("sha256:{}", "a".repeat(64)),
        input_digest: format!("sha256:{}", "b".repeat(64)),
        contract_digest: crate::gateway::routing::assessment::contract_digest(),
        history_complete: false,
        history_truncated: false,
        route_class: RouteClass::new(task_family, next_step_role, Risk::Normal),
        judgments: labels
            .into_iter()
            .map(|(id, label)| {
                (
                    id.into(),
                    Judgment {
                        label: label.into(),
                        confidence: 0.99,
                        probabilities: BTreeMap::from([(label.into(), 1.0)]),
                        accepted: true,
                    },
                )
            })
            .collect(),
    }
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

fn assistant_text(text: &str) -> Message {
    Message::text(Role::Assistant, text)
}

fn a_tool() -> Tool {
    Tool::Function {
        name: "read_file".to_string(),
        description: None,
        parameters: serde_json::json!({"type": "object"}),
        strict: None,
        provider_metadata: ProviderMetadata::new(),
    }
}

fn fusion_declaration() -> Tool {
    Tool::ProviderDefined {
        id: "bitrouter.fusion".to_string(),
        name: "fusion".to_string(),
        args: serde_json::json!({}),
        provider_metadata: ProviderMetadata::new(),
    }
}

fn route(model: &str, messages: Vec<Message>, tools: Vec<Tool>, assessment: &Assessment) -> String {
    let mut p = prompt(model);
    p.messages = messages;
    p.tools = tools;
    router().apply(&mut p, assessment);
    p.model
}

fn read_step() -> Vec<Message> {
    vec![user("fix the bug"), assistant_calls("read_file")]
}

fn completed_read_step() -> Vec<Message> {
    vec![
        user("fix the bug"),
        assistant_calls("read_file"),
        Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id: "call_read_file".to_string(),
                tool_name: None,
                output: ToolResultOutput::Text {
                    value: "source contents".to_string(),
                },
                dynamic: false,
                provider_metadata: ProviderMetadata::new(),
            }],
        },
    ]
}

fn table() -> PolicyTable {
    PolicyTable {
        tiers: HashMap::from([
            ("cheap".into(), Target::from("vendor/cheap")),
            ("flagship".into(), Target::from("vendor/flagship")),
        ]),
        routes: HashMap::from([
            ("unknown|orchestrate|normal".into(), "flagship".into()),
            ("unknown|implement|normal".into(), "cheap".into()),
        ]),
        default_tier: Some("flagship".into()),
        tool_use_tier: Some("flagship".into()),
        tool_safe_tiers: vec!["flagship".into()],
    }
}
fn router() -> PolicyTableRouter {
    PolicyTableRouter::new(Arc::new(table()))
}
impl PolicyTableRouter {
    fn assessed_decision(
        &self,
        prompt: &Prompt,
        headers: &HeaderMap,
        assessment: &Assessment,
    ) -> PolicyDecision {
        self.decision_for_inner(prompt, headers, true, true, Some(assessment))
    }
}

#[test]
fn opening_request_routes_to_its_tier() {
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    // The explicit fix instruction predicts implementation → cheap.
    assert_eq!(
        route("inbound", vec![user("fix the bug")], vec![], &assessment),
        "vendor/cheap"
    );
}

#[test]
fn tool_use_guardrail_clamps_a_non_tool_safe_tier() {
    let assessment = semantic_fixture(TaskFamily::Unknown, NextStep::Unknown);
    // `after_read_file` would route to cheap, but the request carries tools
    // and cheap is not tool-safe → clamped up to the tool_use_tier
    // (flagship). The guardrail is the key safety property.
    assert_eq!(
        route(
            "inbound",
            vec![user("fix the bug"), assistant_calls("read_file")],
            vec![a_tool()],
            &assessment
        ),
        "vendor/flagship"
    );
}

#[test]
fn explicit_provider_route_is_left_untouched() {
    let assessment = semantic_fixture(TaskFamily::Unknown, NextStep::Unknown);
    // A `provider:model` pin (and the `claude-code:` subscription route) is
    // the caller's deliberate choice and is never re-tiered.
    assert_eq!(
        route("vendor:exact-model", vec![user("hi")], vec![], &assessment),
        "vendor:exact-model"
    );
    assert_eq!(
        route(
            "claude-code:claude-opus-4-8",
            vec![user("hi")],
            vec![],
            &assessment
        ),
        "claude-code:claude-opus-4-8"
    );
}

#[test]
fn preset_routes_are_left_for_stage_zero_resolution() {
    let assessment = semantic_fixture(TaskFamily::Unknown, NextStep::Unknown);
    assert_eq!(
        route("@coding", vec![user("hi")], vec![], &assessment),
        "@coding"
    );
    assert_eq!(
        route("@coding:free", vec![user("hi")], vec![], &assessment),
        "@coding:free"
    );
}

#[test]
fn idempotent_on_second_application() {
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    // Applying twice must not double-route: the second pass is already on
    // the tier's model and no-ops.
    let mut p = prompt("inbound");
    p.messages = vec![user("fix the bug")];
    assert!(router().apply(&mut p, &assessment), "first pass routes");
    assert_eq!(p.model, "vendor/cheap");
    assert!(
        !router().apply(&mut p, &assessment),
        "second pass is a no-op"
    );
    assert_eq!(p.model, "vendor/cheap");
}

#[test]
fn prediction_confidence_ppm_clamps_a_literal_edge_case_table() {
    let cases = [
        (f64::NAN, 0),
        (f64::INFINITY, 1_000_000),
        (f64::NEG_INFINITY, 0),
        (-0.25, 0),
        (0.0, 0),
        (0.9, 900_000),
        (1.1, 1_000_000),
    ];

    for (confidence, expected) in cases {
        assert_eq!(prediction_confidence_ppm(confidence), expected);
    }
}

#[test]
fn latest_user_turn_routes_by_semantic_role() {
    let assessment = semantic_fixture(TaskFamily::CodeGeneration, NextStep::Implement);
    // The decisive new user instruction routes to implementation instead
    // of consulting the preceding read-tool call.
    let routed = route(
        "inbound",
        vec![
            user("fix the bug"),
            assistant_calls("read_file"),
            assistant_text("here is what I found"),
            user("Implement a new parser module now."),
        ],
        vec![],
        &assessment,
    );
    assert_eq!(routed, "vendor/cheap");
}

#[test]
fn fusion_declaration_is_left_untouched() {
    let assessment = semantic_fixture(TaskFamily::CodeReview, NextStep::Verify);
    // A request carrying the fusion alias's injected declaration is owned by
    // the fusion flow; the policy table must not re-tier its outer model,
    // even though the model is colonless and the request carries tools.
    assert_eq!(
        route(
            "vendor/fusion-outer",
            vec![user("compare these")],
            vec![fusion_declaration()],
            &assessment
        ),
        "vendor/fusion-outer"
    );
}

#[test]
fn continuation_pin_records_the_predictive_proposal_and_serving_adjustment() {
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    let router = router();
    let mut p = prompt("inbound");
    p.messages = completed_read_step();
    let mut decision = router.assessed_decision(&p, &HeaderMap::new(), &assessment);

    let applied = router.apply_continuation_adjustment(
        &mut decision,
        &ContinuationAdjustment::Pin {
            effective_model: "vendor/flagship".to_owned(),
            effective_effort: None,
            effort_authoritative: true,
        },
    );

    assert!(applied.is_ok());
    assert_eq!(
        decision.continuation_proposed_tier.as_deref(),
        Some("cheap")
    );
    assert_eq!(
        decision.continuation_proposed_model.as_deref(),
        Some("vendor/cheap")
    );
    assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
    assert_eq!(decision.selected_model.as_deref(), Some("vendor/flagship"));
    assert_eq!(decision.continuation_adjustment.as_deref(), Some("pin"));
    assert_eq!(decision.reason, PolicyDecisionReason::ContinuationPin);
    assert_eq!(
        decision
            .route_measurement
            .as_ref()
            .map(|measurement| measurement.logging_action_tier.as_str()),
        Some("cheap")
    );
}

#[test]
fn continuation_detach_records_adjustment_without_rewriting_prediction() {
    let assessment = semantic_fixture(TaskFamily::Unknown, NextStep::Unknown);
    let router = router();
    let mut p = prompt("inbound");
    p.messages = read_step();
    let mut decision = router.assessed_decision(&p, &HeaderMap::new(), &assessment);

    let applied =
        router.apply_continuation_adjustment(&mut decision, &ContinuationAdjustment::Detach);

    assert!(applied.is_ok());
    assert_eq!(
        decision.continuation_proposed_model,
        decision.selected_model
    );
    assert_eq!(decision.continuation_adjustment.as_deref(), Some("detach"));
    assert_eq!(decision.reason, PolicyDecisionReason::StaticTable);
}

#[test]
fn continuation_pin_fails_closed_when_the_active_lock_removed_its_model() {
    let assessment = semantic_fixture(TaskFamily::Unknown, NextStep::Unknown);
    let router = router();
    let mut p = prompt("inbound");
    p.messages = read_step();
    let mut decision = router.assessed_decision(&p, &HeaderMap::new(), &assessment);
    let original_selected = decision.selected_model.clone();

    let error = router.apply_continuation_adjustment(
        &mut decision,
        &ContinuationAdjustment::Pin {
            effective_model: "unavailable-provider:unavailable-model".to_owned(),
            effective_effort: None,
            effort_authoritative: true,
        },
    );

    assert!(matches!(
        error,
        Err(crate::gateway::error::BitrouterError::BadRequest { ref message })
            if message.contains("unavailable in the active policy")
    ));
    assert_eq!(decision.selected_model, original_selected);
}

#[test]
fn decision_reason_tool_guardrail() {
    let assessment = semantic_fixture(TaskFamily::CodeDebugging, NextStep::Implement);
    let router = router();
    let mut p = prompt("inbound");
    p.messages = completed_read_step();
    p.tools = vec![a_tool()];

    let decision = router.assessed_decision(&p, &HeaderMap::new(), &assessment);

    assert_eq!(decision.reason, PolicyDecisionReason::ToolGuardrail);
    assert_eq!(decision.static_tier.as_deref(), Some("cheap"));
    assert_eq!(decision.selected_tier.as_deref(), Some("flagship"));
}
