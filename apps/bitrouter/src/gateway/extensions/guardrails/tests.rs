//! Phase-4 guardrail tests: upstream Block deny + downstream Redact / Abort.

use std::sync::Arc;

use crate::gateway::caller::CallerContext;
use crate::gateway::pipeline::builder::PipelineBuilder;
use crate::gateway::pipeline::context::PipelineContext;
use crate::gateway::pipeline::executor::{MockExecutor, MockResponse};
use crate::gateway::pipeline::hooks::PreRequestHook;
use crate::gateway::pipeline::routing::StaticRoutingTable;
use crate::gateway::pipeline::types::{PipelineRequest, RoutingTarget};
use bitrouter_ai::types::{
    ApiProtocol, FinishReason, GenerationParams, Message, Prompt, Role, StreamPart, Usage,
};
use futures::StreamExt;

use super::hooks::{DepositRulesHook, GuardrailPreHook, GuardrailStreamHook};
use bitrouter_guardrails::rules::{Action, GuardrailRule, RuleSet};

fn rules() -> TestResult<RuleSet> {
    Ok(RuleSet::from_rules([
        GuardrailRule::new("ssn", r"\d{3}-\d{2}-\d{4}", Action::Redact)?,
        GuardrailRule::new("badword", r"(?i)forbidden", Action::Block)?,
    ]))
}

fn prompt(text: &str, stream: bool) -> Prompt {
    Prompt {
        model: "m".to_string(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, text)],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream,
    }
}

fn ctx(text: &str) -> PipelineContext {
    let req = PipelineRequest::new("m", CallerContext::new("k", "u"), prompt(text, false));
    PipelineContext::new(req)
}

/// A context with the guardrail rule set deposited into its extensions, the way
/// `DepositRulesHook` (or a host's per-account resolver) does at Stage 1.
fn ctx_with_rules(text: &str) -> TestResult<PipelineContext> {
    let mut c = ctx(text);
    c.insert_extension(Arc::new(rules()?));
    Ok(c)
}

// ===== upstream: GuardrailPreHook =====

#[tokio::test]
async fn pre_hook_allows_clean_request() -> TestResult {
    let hook = GuardrailPreHook::new();
    let mut c = ctx_with_rules("a perfectly normal question")?;
    assert!(matches!(
        hook.check(&mut c).await?,
        crate::gateway::pipeline::hooks::HookDecision::Allow
    ));
    Ok(())
}

#[tokio::test]
async fn pre_hook_allows_when_no_rules_deposited() -> TestResult {
    // No extension inserted — the hook is a pure consumer and must no-op.
    let hook = GuardrailPreHook::new();
    let mut c = ctx("please do the FORBIDDEN thing");
    assert!(matches!(
        hook.check(&mut c).await?,
        crate::gateway::pipeline::hooks::HookDecision::Allow
    ));
    Ok(())
}

#[tokio::test]
async fn pre_hook_blocks_forbidden_request() -> TestResult {
    let hook = GuardrailPreHook::new();
    let mut c = ctx_with_rules("please do the FORBIDDEN thing")?;
    match hook.check(&mut c).await? {
        crate::gateway::pipeline::hooks::HookDecision::Deny(reason) => {
            let err: crate::gateway::error::BitrouterError = reason.into();
            assert_eq!(err.status(), 400);
        }
        crate::gateway::pipeline::hooks::HookDecision::Allow => {
            return Err("forbidden request must be blocked".into());
        }
    }
    Ok(())
}

// ===== downstream: GuardrailStreamHook via a real streaming pipeline =====

fn target() -> RoutingTarget {
    RoutingTarget {
        provider_name: "p".to_string(),
        service_id: "m".to_string(),
        api_base: "https://example.invalid".to_string(),
        api_key: "k".to_string(),
        api_protocol: ApiProtocol::ChatCompletions,
        chat_token_limit_field: None,
        chat_supports_store: None,
        chat_supports_stream_options: None,
        chat_google_extensions: false,
        reasoning_effort: None,
        model_constraints: Default::default(),
        account_label: None,
        api_key_override: None,
        api_base_override: None,
        auth_scheme: Default::default(),
        headers: Vec::new(),
    }
}

fn routing_table() -> Arc<StaticRoutingTable> {
    let rt = Arc::new(StaticRoutingTable::new());
    rt.insert("m", vec![target()]);
    rt
}

async fn run_stream(
    parts: Vec<StreamPart>,
) -> TestResult<Vec<crate::gateway::error::Result<StreamPart>>> {
    let mut b = PipelineBuilder::new();
    // The deposit hook runs at Stage 1 and the rule set rides the extensions
    // into the StreamContext — end-to-end proof the propagation works.
    b.routing_table(routing_table())
        .executor(Arc::new(MockExecutor::new(vec![MockResponse::Stream(
            parts,
        )])))
        .pre_request_hook(DepositRulesHook::new(Arc::new(rules()?)))
        .stream_hook(GuardrailStreamHook::new());
    let pipeline = Arc::new(b.build()?);
    let req = PipelineRequest::new("m", CallerContext::new("k", "u"), prompt("go", true));
    let stream = pipeline.execute_stream(req).await?;
    Ok(stream.collect().await)
}

#[tokio::test]
async fn stream_hook_redacts_sensitive_text() -> TestResult {
    let parts = run_stream(vec![
        StreamPart::TextDelta {
            text: "your ssn is 123-45-6789 ok".to_string(),
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ])
    .await?;
    let text: String = parts
        .into_iter()
        .filter_map(|p| match p.ok()? {
            StreamPart::TextDelta { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(text, "your ssn is [REDACTED] ok");
    Ok(())
}

#[tokio::test]
async fn stream_hook_aborts_on_blocked_content() -> TestResult {
    let parts = run_stream(vec![
        StreamPart::TextDelta {
            text: "this is fine ".to_string(),
        },
        StreamPart::TextDelta {
            text: "but this is FORBIDDEN".to_string(),
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ])
    .await?;
    // the stream ends in an error (the abort), and the forbidden text is never
    // emitted as a clean delta
    assert!(
        parts.last().is_some_and(Result::is_err),
        "stream aborts on block"
    );
    let clean_text: String = parts
        .iter()
        .filter_map(|p| match p {
            Ok(StreamPart::TextDelta { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(!clean_text.contains("FORBIDDEN"));
    Ok(())
}

#[tokio::test]
async fn stream_hook_passes_clean_content_through() -> TestResult {
    let parts = run_stream(vec![
        StreamPart::TextDelta {
            text: "hello ".to_string(),
        },
        StreamPart::TextDelta {
            text: "world".to_string(),
        },
        StreamPart::Usage {
            usage: Usage {
                prompt_tokens: 1,
                completion_tokens: 2,
                reasoning_tokens: 0,
                ..Default::default()
            },
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ])
    .await?;
    assert!(parts.iter().all(|p| p.is_ok()));
    let text: String = parts
        .into_iter()
        .filter_map(|p| match p.ok()? {
            StreamPart::TextDelta { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello world");
    Ok(())
}

type TestResult<T = ()> = Result<T, Box<dyn std::error::Error>>;

struct CountingExecutor {
    inner: MockExecutor,
    calls: std::sync::atomic::AtomicUsize,
}

#[async_trait::async_trait]
impl crate::gateway::pipeline::executor::Executor for CountingExecutor {
    async fn execute(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<crate::gateway::pipeline::types::ExecutionResult> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.execute(target, prompt, ctx).await
    }

    async fn execute_stream(
        &self,
        target: &RoutingTarget,
        prompt: &Prompt,
        ctx: &PipelineContext,
    ) -> crate::gateway::error::Result<crate::gateway::pipeline::executor::StreamPartStream> {
        self.calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        self.inner.execute_stream(target, prompt, ctx).await
    }
}

fn scripted_executor(texts: &[&str]) -> Arc<CountingExecutor> {
    Arc::new(CountingExecutor {
        inner: MockExecutor::new(
            texts
                .iter()
                .map(|text| {
                    MockResponse::Stream(vec![
                        StreamPart::TextDelta {
                            text: (*text).to_owned(),
                        },
                        StreamPart::Finish {
                            reason: FinishReason::Stop,
                        },
                    ])
                })
                .collect(),
        ),
        calls: std::sync::atomic::AtomicUsize::new(0),
    })
}

fn output_text(parts: &[crate::gateway::error::Result<StreamPart>]) -> String {
    parts
        .iter()
        .filter_map(|part| match part {
            Ok(StreamPart::TextDelta { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect()
}

#[tokio::test]
async fn static_plugin_install_preserves_denial_and_stream_protection() -> TestResult {
    let executor = scripted_executor(&["ssn 123-45-6789", "FORBIDDEN"]);
    let app = crate::gateway::assembly::App::builder()
        .language_model(|lm| {
            lm.routing_table(routing_table()).executor(executor.clone());
        })
        .plugin(super::plugin::GuardrailsPlugin::with_static(rules()?))
        .build()?;
    let pipeline = app.language_model().ok_or("missing generation pipeline")?;
    let request = |text, stream| {
        PipelineRequest::new("m", CallerContext::new("k", "u"), prompt(text, stream))
    };

    assert!(pipeline.execute(request("FORBIDDEN", false)).await.is_err());
    assert!(
        pipeline
            .clone()
            .execute_stream(request("FORBIDDEN", true))
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    let redacted: Vec<_> = pipeline
        .clone()
        .execute_stream(request("go", true))
        .await?
        .collect()
        .await;
    assert!(redacted.iter().all(Result::is_ok));
    assert_eq!(output_text(&redacted), "ssn [REDACTED]");
    let blocked: Vec<_> = pipeline
        .clone()
        .execute_stream(request("go", true))
        .await?
        .collect()
        .await;
    assert!(blocked.last().is_some_and(Result::is_err));
    assert!(!output_text(&blocked).contains("FORBIDDEN"));
    assert_eq!(executor.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    Ok(())
}

struct AccountRules(Arc<RuleSet>);

#[async_trait::async_trait]
impl PreRequestHook for AccountRules {
    async fn check(
        &self,
        ctx: &mut PipelineContext,
    ) -> crate::gateway::error::Result<crate::gateway::pipeline::hooks::HookDecision> {
        if ctx.caller().user_id() == "protected" {
            ctx.insert_extension(self.0.clone());
        }
        Ok(crate::gateway::pipeline::hooks::HookDecision::Allow)
    }
}

#[tokio::test]
async fn dynamic_plugin_install_keeps_rules_request_local() -> TestResult {
    let executor = scripted_executor(&["ssn 123-45-6789", "ssn 123-45-6789"]);
    let rules = Arc::new(rules()?);
    let app = crate::gateway::assembly::App::builder()
        .language_model(|lm| {
            lm.routing_table(routing_table())
                .executor(executor.clone())
                .pre_request_hook(AccountRules(rules));
        })
        .plugin(super::plugin::GuardrailsPlugin::dynamic())
        .build()?;
    let pipeline = app.language_model().ok_or("missing generation pipeline")?;
    let request =
        |user, text| PipelineRequest::new("m", CallerContext::new("k", user), prompt(text, true));
    assert!(
        pipeline
            .clone()
            .execute_stream(request("protected", "FORBIDDEN"))
            .await
            .is_err()
    );
    assert_eq!(executor.calls.load(std::sync::atomic::Ordering::SeqCst), 0);

    for (user, input, expected) in [
        ("protected", "go", "ssn [REDACTED]"),
        ("ordinary", "FORBIDDEN", "ssn 123-45-6789"),
    ] {
        let parts: Vec<_> = pipeline
            .clone()
            .execute_stream(request(user, input))
            .await?
            .collect()
            .await;
        assert!(parts.iter().all(Result::is_ok));
        assert_eq!(output_text(&parts), expected);
    }
    assert_eq!(executor.calls.load(std::sync::atomic::Ordering::SeqCst), 2);
    Ok(())
}
