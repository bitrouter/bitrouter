//! Phase-4 guardrail tests: upstream Block deny + downstream Redact / Abort.

use std::sync::Arc;

use bitrouter_sdk::caller::CallerContext;
use bitrouter_sdk::language_model::{
    ApiProtocol, FinishReason, GenerationParams, Message, MockExecutor, MockResponse,
    PipelineBuilder, PipelineContext, PipelineRequest, PreRequestHook, Prompt, Role, RoutingTarget,
    StaticRoutingTable, StreamPart, Usage,
};
use futures::StreamExt;

use crate::hooks::{GuardrailPreHook, GuardrailStreamHook};
use crate::rules::{Action, GuardrailRule, RuleSet};

fn rules() -> RuleSet {
    RuleSet::from_rules([
        GuardrailRule::new("ssn", r"\d{3}-\d{2}-\d{4}", Action::Redact).unwrap(),
        GuardrailRule::new("badword", r"(?i)forbidden", Action::Block).unwrap(),
    ])
}

fn prompt(text: &str, stream: bool) -> Prompt {
    Prompt {
        model: "m".to_string(),
        system: None,
        messages: vec![Message::text(Role::User, text)],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        stream,
    }
}

fn ctx(text: &str) -> PipelineContext {
    let req = PipelineRequest::new("m", CallerContext::new("k", "u"), prompt(text, false));
    PipelineContext::new(req)
}

// ===== upstream: GuardrailPreHook =====

#[tokio::test]
async fn pre_hook_allows_clean_request() {
    let hook = GuardrailPreHook::new(rules());
    let mut c = ctx("a perfectly normal question");
    assert!(matches!(
        hook.check(&mut c).await.unwrap(),
        bitrouter_sdk::language_model::HookDecision::Allow
    ));
}

#[tokio::test]
async fn pre_hook_blocks_forbidden_request() {
    let hook = GuardrailPreHook::new(rules());
    let mut c = ctx("please do the FORBIDDEN thing");
    match hook.check(&mut c).await.unwrap() {
        bitrouter_sdk::language_model::HookDecision::Deny(reason) => {
            let err: bitrouter_sdk::BitrouterError = reason.into();
            assert_eq!(err.status(), 400);
        }
        bitrouter_sdk::language_model::HookDecision::Allow => {
            panic!("forbidden request must be blocked")
        }
    }
}

// ===== downstream: GuardrailStreamHook via a real streaming pipeline =====

fn target() -> RoutingTarget {
    RoutingTarget {
        provider_name: "p".to_string(),
        service_id: "m".to_string(),
        api_base: "https://example.invalid".to_string(),
        api_key: "k".to_string(),
        api_protocol: ApiProtocol::Openai,
        api_key_override: None,
        api_base_override: None,
    }
}

fn routing_table() -> Arc<StaticRoutingTable> {
    let rt = Arc::new(StaticRoutingTable::new());
    rt.insert("m", vec![target()]);
    rt
}

async fn run_stream(parts: Vec<StreamPart>) -> Vec<bitrouter_sdk::Result<StreamPart>> {
    let mut b = PipelineBuilder::new();
    b.routing_table(routing_table())
        .executor(Arc::new(MockExecutor::new(vec![MockResponse::Stream(
            parts,
        )])))
        .stream_hook(GuardrailStreamHook::new(rules()));
    let pipeline = Arc::new(b.build().unwrap());
    let req = PipelineRequest::new("m", CallerContext::new("k", "u"), prompt("go", true));
    let stream = pipeline.execute_stream(req).await.unwrap();
    stream.collect().await
}

#[tokio::test]
async fn stream_hook_redacts_sensitive_text() {
    let parts = run_stream(vec![
        StreamPart::TextDelta {
            text: "your ssn is 123-45-6789 ok".to_string(),
        },
        StreamPart::Finish {
            reason: FinishReason::Stop,
        },
    ])
    .await;
    let text: String = parts
        .into_iter()
        .filter_map(|p| match p.ok()? {
            StreamPart::TextDelta { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(text, "your ssn is [REDACTED] ok");
}

#[tokio::test]
async fn stream_hook_aborts_on_blocked_content() {
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
    .await;
    // the stream ends in an error (the abort), and the forbidden text is never
    // emitted as a clean delta
    assert!(parts.last().unwrap().is_err(), "stream aborts on block");
    let clean_text: String = parts
        .iter()
        .filter_map(|p| match p {
            Ok(StreamPart::TextDelta { text }) => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(!clean_text.contains("FORBIDDEN"));
}

#[tokio::test]
async fn stream_hook_passes_clean_content_through() {
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
    .await;
    assert!(parts.iter().all(|p| p.is_ok()));
    let text: String = parts
        .into_iter()
        .filter_map(|p| match p.ok()? {
            StreamPart::TextDelta { text } => Some(text),
            _ => None,
        })
        .collect();
    assert_eq!(text, "hello world");
}
