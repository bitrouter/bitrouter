//! Observer/custody integration without a database, provider, or live service.

use std::sync::Arc;
use std::time::Duration;

use anyhow::{Context, Result, ensure};
use bitrouter_ai::types::{ApiProtocol, Content, FinishReason, GenerateResult, Prompt, StreamPart};

use super::Engine;
use crate::auth::events::Authenticated;
use crate::gateway::caller::CallerContext;
use crate::gateway::error::BitrouterError;
use crate::gateway::observability::decision::{EvalInvocation, PendingEvalDecisionStore};
use crate::gateway::observability::prediction::{
    ObservedActionClass, PredictionObservation, PredictiveResponseObserver,
};
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::hooks::{HopOutcome, ObserveHook, Phase, RequestOutcome};
use crate::gateway::pipeline::routing::ModelSelector;
use crate::gateway::pipeline::stream::{StreamOutcome, StreamProcessor};
use crate::gateway::pipeline::types::{ExecutionResult, PipelineRequest, RoutingTarget};
use crate::gateway::shared::admitted::SharedAdmission;
use crate::gateway::shared::authority::Authority;
use crate::gateway::shared::capture::{Inputs, Requirements};
use crate::gateway::shared::crypto::DeploymentKey;
use crate::gateway::shared::ownership::Custody;
use crate::gateway::shared::store::Store;
use crate::policy_lock::{PolicyLock, PolicyRuntime};

struct Fixture {
    context: PipelineContext,
    custody: Arc<Custody>,
    pending: PendingEvalDecisionStore,
    observer: PredictiveResponseObserver,
    invocation: Arc<EvalInvocation>,
}

impl Fixture {
    async fn new() -> Result<Self> {
        // A lazy pool with no minimum connections is closed before use. Its
        // backend tag satisfies Store's constructor, but no SQL can succeed.
        let mut options =
            sea_orm::ConnectOptions::new("postgres://unused:unused@127.0.0.1:1/unused");
        options
            .connect_lazy(true)
            .min_connections(0)
            .max_connections(1);
        let db = sea_orm::Database::connect(options).await?;
        db.clone().close().await?;
        let engine = Engine {
            authority: Authority::new(
                Store::new(db, "observer-tests", Duration::from_secs(1))?,
                DeploymentKey::from_bytes(rand::random())?,
            ),
            owner: "observer-fixture".into(),
            lease_ms: 30_000,
        };
        let config = crate::config::parse(
            "inherit_defaults: false\npresets:\n  coding:\n    model: fixture:strong\n    policy: auto\n",
        )?;
        let document: PolicyLock =
            serde_saphyr::from_str(include_str!("../../../../policy/default.yaml"))?;
        let pending = PendingEvalDecisionStore::default();
        let requirements = Requirements::from_config(&config, Some(&document))?;
        let runtime = PolicyRuntime::from_document(&config, Some(document), pending.clone(), None)?;
        let mut execution = crate::metering::shared::tests::fixture()?.event.execution;
        execution.state = bitrouter_ai::gateway::execution::ExecutionState::Accepted;
        execution.attempts.clear();
        let custody = Custody::fixture(engine, execution.execution_id.clone())?;
        custody.capture.initialize(Inputs::new(
            requirements,
            &execution.execution_id,
            "tenant",
            execution.operation,
        )?)?;
        let admission = SharedAdmission::with_custody(
            custody.clone(),
            "unused".into(),
            execution,
            Authenticated {
                api_key_id: "principal".into(),
                user_id: "tenant".into(),
                policy_id: None,
            },
        );
        let mut request = PipelineRequest::new(
            "fixture",
            CallerContext::anonymous(),
            Prompt {
                model: "fixture:strong".into(),
                system: None,
                system_provider_metadata: Default::default(),
                messages: vec![],
                tools: vec![],
                params: Default::default(),
                response_format: None,
                tool_choice: None,
                stream: false,
            },
        );
        admission.attach(&mut request);
        let mut context = PipelineContext::new(request);
        runtime
            .prepare_selection("auto", None, &mut context)
            .await?;
        runtime.select_variant("auto", None, &mut context).await?;
        admission.retain_routing(&context)?;
        admission.route_ready()?;
        let invocation = context
            .extension::<EvalInvocation>()
            .context("missing invocation")?;
        ensure!(pending.peek(&invocation, "tenant").is_some());
        let observer = PredictiveResponseObserver::new(pending.clone());
        observer.after_phase(Phase::Route, &context).await;
        // Neither later callbacks nor terminal projection need this runtime.
        drop(runtime);
        Ok(Self {
            context,
            custody,
            pending,
            observer,
            invocation,
        })
    }

    fn snapshot(&self) -> Result<serde_json::Value> {
        Ok(serde_json::to_value(
            self.custody
                .capture
                .snapshot()
                .context("missing capture")?
                .as_ref(),
        )?)
    }

    fn evict(&self) -> Result<()> {
        ensure!(self.pending.remove(&self.invocation, "tenant"));
        Ok(())
    }
}

fn target() -> RoutingTarget {
    RoutingTarget {
        provider_name: "provider".into(),
        service_id: "model".into(),
        api_base: "https://example.invalid".into(),
        api_key: String::new(),
        api_protocol: ApiProtocol::Responses,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        chat_google_extensions: false,
        reasoning_effort: None,
        model_constraints: Default::default(),
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: crate::gateway::model::target::AuthScheme::XApiKey,
        headers: vec![],
    }
}

fn result() -> ExecutionResult {
    ExecutionResult {
        provider_id: "provider".into(),
        model_id: "model".into(),
        account_label: None,
        result: GenerateResult {
            content: vec![Content::ToolCall {
                id: "PRIVATE_TOOL_ID".into(),
                name: "apply_patch".into(),
                arguments: "PRIVATE_RESPONSE_SENTINEL".into(),
                provider_executed: false,
                dynamic: false,
                provider_metadata: Default::default(),
            }],
            usage: None,
            finish_reason: Some(FinishReason::Stop),
            response_id: None,
            stop_details: None,
            provider_metadata: Default::default(),
        }
        .into(),
        request_duration_ms: 999,
        upstream_duration_ms: Some(888),
        server_tool_calls: vec![],
    }
}

#[tokio::test]
async fn generated_response_survives_cache_removal_and_request_owner_drop() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.evict()?;
    let before = fixture.snapshot()?;
    fixture
        .observer
        .on_hop_end(
            &fixture.context,
            &target(),
            HopOutcome::Generated(&result()),
        )
        .await;
    let after = fixture.snapshot()?;
    ensure!(
        after["response_observation"]
            == serde_json::json!({
                "observed_action":"mutate", "confidence_ppm":850000, "reason_code":"canonical_response_signal"
            })
    );
    for field in ["policy", "routing", "requirements", "phase"] {
        ensure!(after[field] == before[field]);
    }
    ensure!(
        fixture
            .pending
            .peek(&fixture.invocation, "tenant")
            .is_none()
    );
    ensure!(fixture.observer.buffered_request_count() == 0);
    let serialized = serde_json::to_string(&after)?;
    for marker in [
        "PRIVATE",
        "request_duration_ms",
        "upstream_duration_ms",
        "quality.pass",
        "latency.ms",
    ] {
        ensure!(
            !serialized.contains(marker),
            "unexpected captured field: {marker}"
        );
    }
    ensure!(!serialized.contains(&fixture.invocation.token().to_string()));
    let retained = fixture.custody.clone();
    drop(fixture);
    let terminal = retained.capture.terminal()?.context("missing terminal")?;
    ensure!(
        serde_json::to_value(terminal.as_ref())?["response_observation"]
            == after["response_observation"]
    );
    ensure!(retained.capture.terminal()?.as_deref() == Some(terminal.as_ref()));
    Ok(())
}

#[tokio::test]
async fn fallback_failure_adds_nothing_then_generated_response_is_retained() -> Result<()> {
    let fixture = Fixture::new().await?;
    let before = fixture.snapshot()?;
    fixture
        .observer
        .on_hop_end(
            &fixture.context,
            &target(),
            HopOutcome::Failed(&BitrouterError::UpstreamUnavailable),
        )
        .await;
    fixture
        .observer
        .on_hop_end(&fixture.context, &target(), HopOutcome::StreamStarted)
        .await;
    ensure!(fixture.snapshot()? == before);
    fixture.evict()?;
    fixture
        .observer
        .on_hop_end(
            &fixture.context,
            &target(),
            HopOutcome::Generated(&result()),
        )
        .await;
    ensure!(fixture.snapshot()?["response_observation"]["observed_action"] == "mutate");
    fixture
        .observer
        .on_request_end(
            &fixture.context,
            &RequestOutcome::Failed(BitrouterError::UpstreamUnavailable),
        )
        .await;
    ensure!(fixture.snapshot()?["response_observation"]["observed_action"] == "mutate");
    Ok(())
}

#[tokio::test]
async fn both_stream_terminals_capture_before_seal_and_preserve_output() -> Result<()> {
    for terminal in [
        StreamPart::Finish {
            reason: FinishReason::Error("PRIVATE_ERROR".into()),
        },
        StreamPart::ResponseCompleted {
            id: "PRIVATE_NATIVE_ID".into(),
            source_protocol: ApiProtocol::Responses,
            status: "failed".into(),
            usage: None,
            response_output_commitment: None,
        },
    ] {
        let fixture = Fixture::new().await?;
        fixture.evict()?;
        let mut processor = StreamProcessor::new(
            vec![],
            vec![Arc::new(fixture.observer.clone())],
            fixture.context.stream_context(),
        );
        let parts = vec![
            StreamPart::ToolCallDelta {
                id: "private-call".into(),
                name: Some("exec_command".into()),
                arguments: "{\"cmd\":\"rm ".into(),
                provider_metadata: Default::default(),
            },
            StreamPart::ToolCallDelta {
                id: "private-call".into(),
                name: None,
                arguments: "PRIVATE_PATH\"}".into(),
                provider_metadata: Default::default(),
            },
            terminal,
        ];
        let mut output = Vec::new();
        for part in parts.clone() {
            output.extend(processor.process_part(part).await?);
        }
        ensure!(output == parts);
        let before = fixture.snapshot()?;
        ensure!(before["response_observation"]["observed_action"] == "mutate");
        ensure!(!serde_json::to_string(&before)?.contains("PRIVATE"));
        fixture
            .observer
            .on_request_end(&fixture.context, &RequestOutcome::ClientDisconnected)
            .await;
        let sealed = fixture
            .custody
            .capture
            .terminal()?
            .context("missing terminal")?;
        // A duplicate terminal after sealing is ignored: the buffer was removed.
        processor
            .process_part(StreamPart::Finish {
                reason: FinishReason::Stop,
            })
            .await?;
        ensure!(fixture.custody.capture.snapshot().as_deref() == Some(sealed.as_ref()));
        ensure!(fixture.observer.buffered_request_count() == 0);
    }
    Ok(())
}

#[tokio::test]
async fn interrupted_stream_without_observed_terminal_stays_absent() -> Result<()> {
    for outcome in [
        StreamOutcome::ClientDisconnected,
        StreamOutcome::UpstreamError(BitrouterError::UpstreamUnavailable),
    ] {
        let fixture = Fixture::new().await?;
        let mut processor = StreamProcessor::new(
            vec![],
            vec![Arc::new(fixture.observer.clone())],
            fixture.context.stream_context(),
        );
        processor
            .process_part(StreamPart::TextDelta {
                text: "PRIVATE_PARTIAL".into(),
            })
            .await?;
        processor.finish(outcome).await;
        fixture
            .observer
            .on_request_end(&fixture.context, &RequestOutcome::ClientDisconnected)
            .await;
        let sealed = fixture
            .custody
            .capture
            .terminal()?
            .context("missing terminal")?;
        ensure!(
            serde_json::to_value(sealed.as_ref())?
                .get("response_observation")
                .is_none()
        );
        ensure!(fixture.observer.buffered_request_count() == 0);
    }
    Ok(())
}

#[tokio::test]
async fn foreign_stream_identity_and_invocation_owner_cannot_change_custody() -> Result<()> {
    let mut fixture = Fixture::new().await?;
    let before = fixture.snapshot()?;
    let mut stream = fixture.context.stream_context();
    stream.request_id = "foreign-execution".into();
    fixture
        .observer
        .on_stream_part(
            &stream,
            &StreamPart::Finish {
                reason: FinishReason::Stop,
            },
        )
        .await;
    ensure!(fixture.snapshot()? == before);
    let admission = fixture
        .context
        .extension::<SharedAdmission>()
        .context("missing admission")?;
    let observation = PredictionObservation::new(ObservedActionClass::Mutate);
    for caller in [
        CallerContext::new("other-key", "tenant"),
        CallerContext::new("principal", "other-owner"),
        CallerContext::local(),
    ] {
        ensure!(
            admission
                .retain_response_observation(
                    fixture.context.request_id(),
                    &caller,
                    &fixture.invocation,
                    observation
                )
                .is_err()
        );
    }
    fixture
        .context
        .insert_extension(Arc::new(EvalInvocation::new("foreign-owner")));
    fixture
        .observer
        .on_hop_end(
            &fixture.context,
            &target(),
            HopOutcome::Generated(&result()),
        )
        .await;
    ensure!(fixture.snapshot()? == before);
    Ok(())
}

#[tokio::test]
async fn late_generated_callback_cannot_rewrite_sealed_absence() -> Result<()> {
    let fixture = Fixture::new().await?;
    let sealed = fixture
        .custody
        .capture
        .terminal()?
        .context("missing terminal")?;
    fixture
        .observer
        .on_hop_end(
            &fixture.context,
            &target(),
            HopOutcome::Generated(&result()),
        )
        .await;
    ensure!(fixture.custody.capture.snapshot().as_deref() == Some(sealed.as_ref()));
    // The local optional cache keeps its original behavior even when shared
    // custody rejects a late callback. It is not read back into projection.
    ensure!(
        fixture
            .pending
            .peek(&fixture.invocation, "tenant")
            .and_then(|decision| decision.observation)
            .map(|observation| observation.observed_action)
            == Some(ObservedActionClass::Mutate)
    );
    Ok(())
}

#[tokio::test]
async fn detached_stream_keeps_the_admission_handoff_without_request_or_cache() -> Result<()> {
    let fixture = Fixture::new().await?;
    fixture.evict()?;
    let mut processor = StreamProcessor::new(
        vec![],
        vec![Arc::new(fixture.observer.clone())],
        fixture.context.stream_context(),
    );
    let custody = fixture.custody.clone();
    drop(fixture);
    processor
        .process_part(StreamPart::TextDelta {
            text: "PRIVATE_TEXT".into(),
        })
        .await?;
    processor
        .process_part(StreamPart::Finish {
            reason: FinishReason::Stop,
        })
        .await?;
    drop(processor);
    let terminal = custody.capture.terminal()?.context("missing terminal")?;
    let value = serde_json::to_value(terminal.as_ref())?;
    ensure!(value["response_observation"]["observed_action"] == "answer_or_summarize");
    ensure!(!serde_json::to_string(&value)?.contains("PRIVATE_TEXT"));
    Ok(())
}
