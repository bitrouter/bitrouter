use std::collections::{BTreeMap, BTreeSet};

use bitrouter::eval::EvalService;
use bitrouter::eval::admission::SubmissionPrincipal;
use bitrouter::eval::compiler::EvalEvidenceSnapshot;
use bitrouter::eval::settlement::{
    EvalInvocation, EvalSettlementRecorder, PendingEvalDecision, PendingEvalDecisionStore,
};
use bitrouter::eval::store::EvalStore;
use bitrouter::eval::types::{
    EVAL_SCHEMA_VERSION, EvalDecisionRef, EvalScope, EvalSubject, EvalVerdict, EvaluationResult,
    EvaluatorIdentity, EvaluatorKind, evidence_digest,
};
use bitrouter::policy_compile::{CompileInput, LegacyAdequacySnapshot, compile_candidate};
use bitrouter::policy_lock::{PolicyDefinition, PolicyLock, deterministic_yaml, semantic_digest};
use bitrouter::workflow_state::response_observer::PredictiveResponseObserver;
use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::event::EventBus;
use bitrouter_sdk::language_model::types::AuthScheme;
use bitrouter_sdk::language_model::{
    ApiProtocol, Content, ExecutionResult, FinishReason, GenerateResult, GenerationParams,
    HopOutcome, ObserveHook, PipelineContext, PipelineRequest, Prompt, RoutingTarget,
    SettlementContext, SettlementRecorder, UsageOrigin,
};

fn base_lock() -> PolicyLock {
    PolicyLock {
        lockfile_version: 1,
        artifact: None,
        policies: BTreeMap::from([(
            "auto".to_string(),
            PolicyDefinition {
                tiers: BTreeMap::from([
                    ("economy".to_string(), "vendor:economy".into()),
                    ("strong".to_string(), "vendor:strong".into()),
                ]),
                default_tier: Some("strong".to_string()),
                tool_use_tier: Some("strong".to_string()),
                tool_safe_tiers: vec!["strong".to_string(), "economy".to_string()],
                ..PolicyDefinition::default()
            },
        )]),
        certificates: BTreeMap::new(),
    }
}

#[test]
fn policy_publish_cli_promotes_the_exact_compiled_candidate() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let active_path = directory.path().join("policy-lock.yaml");
    let candidate_path = directory.path().join("candidate.yaml");
    std::fs::write(
        &config_path,
        r#"policy:
  mode: adaptive
presets:
  auto:
    model: vendor:strong
    policy: auto
"#,
    )?;
    let active = base_lock();
    std::fs::write(&active_path, deterministic_yaml(&active)?)?;
    let legacy = LegacyAdequacySnapshot {
        snapshot_time_unix_ms: 1_785_369_600_000,
        pins: Vec::new(),
        exploration: Vec::new(),
        semantic_successes: Vec::new(),
        reliability_events: Vec::new(),
    };
    let candidate = compile_candidate(CompileInput {
        current: &active,
        parent_digest: Some(&semantic_digest(&active)?),
        legacy: &legacy,
        eval: None,
        proposed_progress_guards: None,
    })?
    .document;
    std::fs::write(&candidate_path, deterministic_yaml(&candidate)?)?;

    let output = std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args([
            "policy",
            "publish",
            candidate_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("candidate path is not UTF-8"))?,
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("config path is not UTF-8"))?,
        ])
        .output()?;

    assert!(
        output.status.success(),
        "publish failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    let published: PolicyLock = serde_saphyr::from_str(&std::fs::read_to_string(active_path)?)?;
    assert_eq!(semantic_digest(&published)?, semantic_digest(&candidate)?);
    Ok(())
}

#[test]
fn frozen_policy_rejects_candidate_without_touching_active_bytes() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let active_path = directory.path().join("policy-lock.yaml");
    let candidate_path = directory.path().join("candidate.yaml");
    std::fs::write(
        &config_path,
        "policy:\n  mode: frozen\npresets:\n  auto:\n    model: vendor:strong\n    policy: auto\n",
    )?;
    let active = base_lock();
    let active_bytes = deterministic_yaml(&active)?;
    std::fs::write(&active_path, &active_bytes)?;
    std::fs::write(&candidate_path, deterministic_yaml(&compile(&active)?)?)?;

    let output = publish_command(&candidate_path, &config_path)?;

    assert!(!output.status.success());
    assert_eq!(std::fs::read_to_string(active_path)?, active_bytes);
    Ok(())
}

#[tokio::test]
async fn stale_candidate_loses_compare_and_swap_without_touching_active_bytes() -> anyhow::Result<()>
{
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let active_path = directory.path().join("policy-lock.yaml");
    let candidate_path = directory.path().join("candidate.yaml");
    std::fs::write(
        &config_path,
        "policy:\n  mode: adaptive\npresets:\n  auto:\n    model: vendor:strong\n    policy: auto\n",
    )?;
    let original = base_lock();
    std::fs::write(&candidate_path, deterministic_yaml(&compile(&original)?)?)?;
    let mut newer = original;
    newer
        .policies
        .get_mut("auto")
        .ok_or_else(|| anyhow::anyhow!("base fixture must contain the auto policy"))?
        .routes
        .insert("agent_trace/v1|edit|normal".into(), "strong".into());
    let newer_bytes = deterministic_yaml(&newer)?;
    std::fs::write(&active_path, &newer_bytes)?;

    let error = bitrouter::policy_lock::publish_candidate_file(&config_path, &candidate_path)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("stale publication unexpectedly succeeded"))?;

    assert_eq!(std::fs::read_to_string(active_path)?, newer_bytes);
    assert!(error.to_string().contains("parent digest"));
    Ok(())
}

#[test]
fn concurrent_publishers_allow_exactly_one_parent_transition() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let active_path = directory.path().join("policy-lock.yaml");
    let history_path = directory.path().join("history");
    let active = base_lock();
    let candidate = compile(&active)?;
    let parent_digest = semantic_digest(&active)?;
    std::fs::write(&active_path, deterministic_yaml(&active)?)?;
    let barrier = std::sync::Arc::new(std::sync::Barrier::new(3));

    let publishers = (0..2)
        .map(|_| {
            let active_path = active_path.clone();
            let history_path = history_path.clone();
            let candidate = candidate.clone();
            let parent_digest = parent_digest.clone();
            let barrier = barrier.clone();
            std::thread::spawn(move || {
                barrier.wait();
                bitrouter::policy_lock::publish_candidate(
                    &active_path,
                    &parent_digest,
                    &candidate,
                    &history_path,
                )
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();

    let mut successes = 0;
    let mut failures = Vec::new();
    for publisher in publishers {
        match publisher.join() {
            Ok(Ok(_)) => successes += 1,
            Ok(Err(error)) => failures.push(error.to_string()),
            Err(_) => anyhow::bail!("publisher thread panicked"),
        }
    }

    assert_eq!(successes, 1);
    assert_eq!(failures.len(), 1);
    assert!(failures[0].contains("policy lock changed"));
    let published: PolicyLock = serde_saphyr::from_str(&std::fs::read_to_string(active_path)?)?;
    assert_eq!(semantic_digest(&published)?, semantic_digest(&candidate)?);
    Ok(())
}

fn compile(active: &PolicyLock) -> anyhow::Result<PolicyLock> {
    let legacy = LegacyAdequacySnapshot {
        snapshot_time_unix_ms: 1_785_369_600_000,
        pins: Vec::new(),
        exploration: Vec::new(),
        semantic_successes: Vec::new(),
        reliability_events: Vec::new(),
    };
    Ok(compile_candidate(CompileInput {
        current: active,
        parent_digest: Some(&semantic_digest(active)?),
        legacy: &legacy,
        eval: None,
        proposed_progress_guards: None,
    })?
    .document)
}

#[tokio::test]
async fn policy_eval_control_plane_records_observed_action_without_quality_reward()
-> anyhow::Result<()> {
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = EvalStore::new(db);
    let pending = PendingEvalDecisionStore::default();
    let invocation = EvalInvocation::new("local");
    pending.insert(
        &invocation,
        PendingEvalDecision {
            request_id: "request-observed".into(),
            decision_id: "decision-observed".into(),
            policy: "auto:cost".into(),
            policy_digest:
                "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef".into(),
            route_projection: "agent_trace/v2|edit|normal".into(),
            request_key: "agent_trace/v2|edit|normal".into(),
            selected_tier: "economy".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            predictive_v1_fallback_tier: None,
            preset: Some("auto:cost".into()),
            holdout: false,
            continuation_proposed_tier: None,
            continuation_proposed_model: None,
            continuation_proposed_effort: None,
            continuation_adjustment: None,
            predicted_role: Some("implement".into()),
            predicted_task_family: None,
            predicted_action: Some("mutate".into()),
            prediction_confidence_ppm: Some(900_000),
            task_family_confidence_ppm: None,
            task_family_reason_codes: Vec::new(),
            predictor_contract_digest: Some(
                "sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec".into(),
            ),
            prediction_confidence_kind: Some("heuristic_margin".into()),
            observation: None,
            observed_at: "2026-08-08T00:00:00Z".into(),
        },
    );
    let observer = PredictiveResponseObserver::new(pending.clone());
    let mut context = PipelineContext::new(PipelineRequest {
        request_id: "request-observed".into(),
        model: "model".into(),
        caller: CallerContext::local(),
        headers: http::HeaderMap::new(),
        prompt: Prompt {
            model: "model".into(),
            system: None,
            system_provider_metadata: BTreeMap::new(),
            messages: Vec::new(),
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        },
        inbound_protocol: Some(ApiProtocol::Responses),
    });
    context.emit(invocation.clone());
    context.insert_extension(std::sync::Arc::new(invocation.clone()));
    let execution = ExecutionResult {
        provider_id: "provider".into(),
        model_id: "model".into(),
        account_label: None,
        result: GenerateResult {
            content: vec![Content::ToolCall {
                id: "call-1".into(),
                name: "apply_patch".into(),
                arguments: r#"{"private":"never-persist"}"#.into(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: BTreeMap::new(),
            }],
            usage: None,
            finish_reason: Some(FinishReason::Stop),
            response_id: None,
            stop_details: None,
            provider_metadata: BTreeMap::new(),
        },
        request_duration_ms: 1,
        upstream_duration_ms: Some(1),
        server_tool_calls: Vec::new(),
    };
    observer
        .on_hop_end(
            &context,
            &RoutingTarget {
                provider_name: "provider".into(),
                service_id: "model".into(),
                api_base: "https://example.invalid".into(),
                api_key: String::new(),
                api_protocol: ApiProtocol::Responses,
                chat_token_limit_field: None,
                chat_supports_store: None,
                chat_supports_stream_options: None,
                reasoning_effort: None,
                account_label: None,
                api_key_override: None,
                api_base_override: None,
                auth_scheme: AuthScheme::XApiKey,
            },
            HopOutcome::Generated(&execution),
        )
        .await;
    let recorder = EvalSettlementRecorder::new(
        store.clone(),
        pending,
        std::sync::Arc::new(bitrouter::metering::PricingTable::new()),
    );
    let mut settlement = SettlementContext {
        request_id: "request-observed".into(),
        caller: CallerContext::local(),
        target: None,
        model_id: "model".into(),
        reasoning_effort: None,
        provider_id: "provider".into(),
        account_label: None,
        prompt_tokens: 10,
        completion_tokens: 5,
        reasoning_tokens: 0,
        cache_read_tokens: 0,
        cache_write_tokens: 0,
        usage_origin: UsageOrigin::ProviderReported,
        raw_usage: None,
        web_search_count: 0,
        media_input_count: 0,
        media_output_count: 0,
        server_tool_calls: Vec::new(),
        streamed: false,
        request_duration_ms: 123,
        upstream_duration_ms: Some(100),
        ttft_ms: None,
        generation_duration_ms: None,
        first_token_kind: None,
        finish_reason: Some(FinishReason::Stop),
        error: None,
        events: EventBus::default(),
    };
    settlement.emit(invocation);

    recorder.record(&mut settlement).await?;

    let subject = store
        .subject("request:request-observed")
        .await?
        .ok_or_else(|| anyhow::anyhow!("request eval subject missing"))?;
    let evidence = subject
        .evidence
        .iter()
        .find(|evidence| evidence.evidence_id == "request-outcome")
        .ok_or_else(|| anyhow::anyhow!("request outcome evidence missing"))?;
    assert_eq!(
        evidence
            .attributes
            .get("observed_action")
            .map(String::as_str),
        Some("mutate")
    );
    assert_eq!(
        evidence.attributes.get("action_match").map(String::as_str),
        Some("true")
    );
    assert_eq!(
        evidence
            .attributes
            .get("observation_confidence_ppm")
            .map(String::as_str),
        Some("850000")
    );
    assert_eq!(
        evidence
            .attributes
            .get("observation_reason_code")
            .map(String::as_str),
        Some("canonical_response_signal")
    );
    assert_eq!(
        evidence
            .attributes
            .get("predictor_contract_digest")
            .map(String::as_str),
        Some("sha256:7483fb5fa02c0141f568b82287234895c666fef426789e32783bdd3a00cea3ec")
    );
    assert_eq!(
        evidence
            .attributes
            .get("prediction_confidence_kind")
            .map(String::as_str),
        Some("heuristic_margin")
    );
    assert!(!evidence.attributes.contains_key("quality.pass"));
    assert!(!serde_json::to_string(&subject)?.contains("never-persist"));
    Ok(())
}

fn publish_command(
    candidate_path: &std::path::Path,
    config_path: &std::path::Path,
) -> anyhow::Result<std::process::Output> {
    Ok(std::process::Command::new(env!("CARGO_BIN_EXE_bitrouter"))
        .args([
            "policy",
            "publish",
            candidate_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("candidate path is not UTF-8"))?,
            "--config",
            config_path
                .to_str()
                .ok_or_else(|| anyhow::anyhow!("config path is not UTF-8"))?,
        ])
        .output()?)
}

#[tokio::test]
async fn snapshot_compile_publish_preserves_exact_eval_lineage() -> anyhow::Result<()> {
    let directory = tempfile::tempdir()?;
    let config_path = directory.path().join("bitrouter.yaml");
    let active_path = directory.path().join("policy-lock.yaml");
    let candidate_path = directory.path().join("candidate.yaml");
    std::fs::write(
        &config_path,
        "policy:\n  mode: adaptive\npresets:\n  auto:\n    model: vendor:strong\n    policy: auto\n",
    )?;
    let active = base_lock();
    std::fs::write(&active_path, deterministic_yaml(&active)?)?;
    let db = bitrouter::db::connect("sqlite::memory:").await?;
    bitrouter::db::run_migrations(&db).await?;
    let store = EvalStore::new(db);
    let service = EvalService::new(store.clone(), Default::default());
    let evidence = Vec::new();
    let subject = EvalSubject {
        schema_version: EVAL_SCHEMA_VERSION,
        eval_id: "eval-publication".into(),
        scope: EvalScope::Task,
        subject_id: "task-publication".into(),
        policy_digest: semantic_digest(&active)?,
        preset: Some("auto".into()),
        cohort: None,
        holdout: false,
        decisions: vec![EvalDecisionRef {
            decision_id: "decision-publication".into(),
            policy: "auto".into(),
            route_projection: None,
            request_key: "agent_trace/v1|edit|normal".into(),
            selected_tier: "economy".into(),
            selected_effort: None,
            baseline_tier: Some("strong".into()),
            baseline_effort: None,
            predictive_v1_fallback_tier: None,
            policy_digest: semantic_digest(&active)?,
        }],
        requested_dimensions: BTreeSet::from(["quality.pass".into()]),
        evidence_digest: evidence_digest(&evidence)?,
        evidence,
        observed_at: "2026-07-30T00:00:00Z".into(),
    };
    store.insert_subject(&subject).await?;
    service
        .submit(
            EvaluationResult {
                schema_version: EVAL_SCHEMA_VERSION,
                eval_id: subject.eval_id.clone(),
                evidence_digest: subject.evidence_digest.clone(),
                evaluator: EvaluatorIdentity {
                    authority_id: "task-native".into(),
                    evaluator_id: "terminal-bench".into(),
                    kind: EvaluatorKind::TaskNative,
                    version: "1".into(),
                    config_digest: semantic_digest(&active)?,
                },
                verdict: EvalVerdict::Pass,
                metrics: BTreeMap::new(),
                hard_violations: Vec::new(),
                confidence_ppm: Some(1_000_000),
                evidence_refs: Vec::new(),
                decision_credit: BTreeMap::new(),
                idempotency_key: "result-publication".into(),
                submitted_at: "2026-07-30T00:01:00Z".into(),
            },
            SubmissionPrincipal::LocalOperator,
        )
        .await?;
    let manifest = store
        .freeze_snapshot_for_owner("2026-07-30T00:02:00Z", "local")
        .await?;
    let eval = EvalEvidenceSnapshot::load(&store, &manifest.evidence_root).await?;
    let legacy = LegacyAdequacySnapshot {
        snapshot_time_unix_ms: 1_785_369_600_000,
        pins: Vec::new(),
        exploration: Vec::new(),
        semantic_successes: Vec::new(),
        reliability_events: Vec::new(),
    };
    let candidate = compile_candidate(CompileInput {
        current: &active,
        parent_digest: Some(&semantic_digest(&active)?),
        legacy: &legacy,
        eval: Some(&eval),
        proposed_progress_guards: None,
    })?
    .document;
    std::fs::write(&candidate_path, deterministic_yaml(&candidate)?)?;

    bitrouter::policy_lock::publish_candidate_file(&config_path, &candidate_path).await?;

    let published: PolicyLock = serde_saphyr::from_str(&std::fs::read_to_string(active_path)?)?;
    assert_eq!(semantic_digest(&published)?, semantic_digest(&candidate)?);
    assert_eq!(
        published
            .artifact
            .as_ref()
            .and_then(|artifact| artifact.eval_snapshot_root.as_deref()),
        Some(manifest.evidence_root.as_str())
    );
    assert_eq!(
        published.policies["auto"].routes["agent_trace/v1|edit|normal"],
        "economy"
    );
    Ok(())
}
