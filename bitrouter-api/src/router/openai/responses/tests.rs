use std::sync::Arc;

use bitrouter_core::{
    errors::Result,
    models::language::{
        call_options::LanguageModelCallOptions,
        content::LanguageModelContent,
        finish_reason::LanguageModelFinishReason,
        generate_result::LanguageModelGenerateResult,
        language_model::{DynLanguageModel, LanguageModel},
        stream_part::LanguageModelStreamPart,
        stream_result::LanguageModelStreamResult,
        usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
    },
    observe::{CallerContext, ObserveCallback, RequestFailureEvent, RequestSuccessEvent},
    routers::{
        router::LanguageModelRouter,
        routing_table::{RoutingTable, RoutingTarget},
    },
};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use warp::Filter;

use super::filters::{responses_filter, responses_filter_with_observe};

// ── Mock implementations ────────────────────────────────────────────────────

struct MockTable;
impl RoutingTable for MockTable {
    async fn route(&self, incoming: &str) -> Result<RoutingTarget> {
        Ok(RoutingTarget {
            provider_name: "mock".to_owned(),
            model_id: incoming.to_owned(),
        })
    }
}

struct MockRouter;
impl LanguageModelRouter for MockRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(MockModel {
            model_id: target.model_id,
        }))
    }
}

#[derive(Clone)]
struct MockModel {
    model_id: String,
}

impl LanguageModel for MockModel {
    fn provider_name(&self) -> &str {
        "mock"
    }
    fn model_id(&self) -> &str {
        &self.model_id
    }
    async fn supported_urls(&self) -> HashMap<String, Regex> {
        HashMap::new()
    }
    async fn generate(
        &self,
        _options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        Ok(LanguageModelGenerateResult {
            content: LanguageModelContent::Text {
                text: "Hello from responses!".to_owned(),
                provider_metadata: None,
            },
            finish_reason: LanguageModelFinishReason::Stop,
            usage: LanguageModelUsage {
                input_tokens: LanguageModelInputTokens {
                    total: Some(8),
                    no_cache: None,
                    cache_read: None,
                    cache_write: None,
                },
                output_tokens: LanguageModelOutputTokens {
                    total: Some(4),
                    text: None,
                    reasoning: None,
                },
                raw: None,
            },
            provider_metadata: None,
            request: None,
            response_metadata: None,
            warnings: None,
        })
    }
    async fn stream(
        &self,
        _options: LanguageModelCallOptions,
    ) -> Result<LanguageModelStreamResult> {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let _ = tx
            .send(
                bitrouter_core::models::language::stream_part::LanguageModelStreamPart::TextDelta {
                    id: "0".to_owned(),
                    delta: "Hello".to_owned(),
                    provider_metadata: None,
                },
            )
            .await;
        let _ = tx
            .send(
                bitrouter_core::models::language::stream_part::LanguageModelStreamPart::Finish {
                    usage: LanguageModelUsage {
                        input_tokens: LanguageModelInputTokens {
                            total: Some(8),
                            no_cache: None,
                            cache_read: None,
                            cache_write: None,
                        },
                        output_tokens: LanguageModelOutputTokens {
                            total: Some(4),
                            text: None,
                            reasoning: None,
                        },
                        raw: None,
                    },
                    finish_reason: LanguageModelFinishReason::Stop,
                    provider_metadata: None,
                },
            )
            .await;

        Ok(LanguageModelStreamResult {
            stream: Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            request: None,
            response: None,
        })
    }
}

// ── Tool call mock implementations ──────────────────────────────────────────

fn mock_usage() -> LanguageModelUsage {
    LanguageModelUsage {
        input_tokens: LanguageModelInputTokens {
            total: Some(10),
            no_cache: None,
            cache_read: None,
            cache_write: None,
        },
        output_tokens: LanguageModelOutputTokens {
            total: Some(5),
            text: None,
            reasoning: None,
        },
        raw: None,
    }
}

struct MockToolRouter;
impl LanguageModelRouter for MockToolRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(MockToolModel {
            model_id: target.model_id,
        }))
    }
}

#[derive(Clone)]
struct MockToolModel {
    model_id: String,
}

impl LanguageModel for MockToolModel {
    fn provider_name(&self) -> &str {
        "mock"
    }
    fn model_id(&self) -> &str {
        &self.model_id
    }
    async fn supported_urls(&self) -> HashMap<String, Regex> {
        HashMap::new()
    }
    async fn generate(
        &self,
        _options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        Ok(LanguageModelGenerateResult {
            content: LanguageModelContent::ToolCall {
                tool_call_id: "call_abc123".to_owned(),
                tool_name: "get_weather".to_owned(),
                tool_input: r#"{"location":"Paris"}"#.to_owned(),
                provider_executed: None,
                dynamic: None,
                provider_metadata: None,
            },
            finish_reason: LanguageModelFinishReason::FunctionCall,
            usage: mock_usage(),
            provider_metadata: None,
            request: None,
            response_metadata: None,
            warnings: None,
        })
    }
    async fn stream(
        &self,
        _options: LanguageModelCallOptions,
    ) -> Result<LanguageModelStreamResult> {
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let _ = tx
            .send(LanguageModelStreamPart::ToolCall {
                tool_call_id: "call_abc123".to_owned(),
                tool_name: "get_weather".to_owned(),
                tool_input: r#"{"location":"Paris"}"#.to_owned(),
                provider_executed: None,
                dynamic: None,
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::Finish {
                usage: mock_usage(),
                finish_reason: LanguageModelFinishReason::FunctionCall,
                provider_metadata: None,
            })
            .await;
        Ok(LanguageModelStreamResult {
            stream: Box::pin(tokio_stream::wrappers::ReceiverStream::new(rx)),
            request: None,
            response: None,
        })
    }
}

fn parse_sse_body(body: &[u8]) -> Vec<(Option<String>, String)> {
    String::from_utf8_lossy(body)
        .replace("\r\n", "\n")
        .split("\n\n")
        .filter_map(|frame| {
            let mut event = None;
            let mut data_parts = Vec::new();

            for line in frame.lines() {
                if let Some(value) = line.strip_prefix("event:") {
                    event = Some(value.trim().to_owned());
                } else if let Some(value) = line.strip_prefix("data:") {
                    data_parts.push(value.trim().to_owned());
                }
            }

            if data_parts.is_empty() {
                None
            } else {
                Some((event, data_parts.join("\n")))
            }
        })
        .collect()
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn responses_generate_text_input() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = responses_filter(table, router);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "Say hello"
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/responses")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["object"], "response");
    assert_eq!(json["model"], "gpt-4o");
    assert_eq!(json["status"], "completed");
    assert_eq!(
        json["output"][0]["content"][0]["text"],
        "Hello from responses!"
    );
}

#[tokio::test]
async fn responses_generate_messages_input() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = responses_filter(table, router);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            {"role": "user", "content": "Hello"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/responses")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["output"][0]["role"], "assistant");
}

#[tokio::test]
async fn responses_streaming_sends_sse_events() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = responses_filter(table, router);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "Say hello",
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/responses")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");

    let events = parse_sse_body(res.body());
    assert_eq!(events.len(), 3);

    assert_eq!(events[0].0.as_deref(), Some("response.output_text.delta"));
    let delta: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(delta["type"], "response.output_text.delta");
    assert_eq!(delta["output_index"], 0);
    assert_eq!(delta["content_index"], 0);
    assert_eq!(delta["delta"], "Hello");

    assert_eq!(events[1].0.as_deref(), Some("response.completed"));
    let finish: Value = serde_json::from_str(&events[1].1).unwrap();
    assert_eq!(finish["type"], "response.completed");
    assert!(finish.get("delta").is_none() || finish["delta"].is_null());

    assert_eq!(events[2].0, None);
    assert_eq!(events[2].1, "[DONE]");
}

#[tokio::test]
async fn responses_wrong_method() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = responses_filter(table, router);

    let res = warp::test::request()
        .method("GET")
        .path("/v1/responses")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 405);
}

// ── Mock observer ───────────────────────────────────────────────────────

struct MockObserver {
    success_count: AtomicU64,
    failure_count: AtomicU64,
}

impl MockObserver {
    fn new() -> Self {
        Self {
            success_count: AtomicU64::new(0),
            failure_count: AtomicU64::new(0),
        }
    }
}

impl ObserveCallback for MockObserver {
    fn on_request_success(
        &self,
        _event: RequestSuccessEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.success_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }

    fn on_request_failure(
        &self,
        _event: RequestFailureEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.failure_count.fetch_add(1, Ordering::SeqCst);
        Box::pin(async {})
    }
}

// ── Observe tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn responses_observe_success() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let observer = Arc::new(MockObserver::new());
    let filter = responses_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "Say hello"
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/responses")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(observer.success_count.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn responses_observe_streaming_success() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let observer = Arc::new(MockObserver::new());
    let filter = responses_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "Say hello",
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/responses")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(observer.success_count.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure_count.load(Ordering::SeqCst), 0);
}

// ── Tool call tests ─────────────────────────────────────────────────────

#[tokio::test]
async fn responses_with_tools() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolRouter);
    let filter = responses_filter(table, router);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "What's the weather in Paris?",
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "description": "Get current weather",
            "parameters": {"type": "object", "properties": {"location": {"type": "string"}}}
        }]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/responses")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["status"], "completed");
    let output = &json["output"][0];
    assert_eq!(output["type"], "function_call");
    assert_eq!(output["call_id"], "call_abc123");
    assert_eq!(output["name"], "get_weather");
    assert_eq!(output["arguments"], r#"{"location":"Paris"}"#);
    assert_eq!(output["status"], "completed");
}

#[tokio::test]
async fn responses_function_call_output_multi_turn() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = responses_filter(table, router);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": [
            {"role": "user", "content": "What's the weather?"},
            {"type": "function_call_output", "call_id": "call_abc123", "output": "Sunny, 22°C"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/responses")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["status"], "completed");
    assert_eq!(
        json["output"][0]["content"][0]["text"],
        "Hello from responses!"
    );
}

#[tokio::test]
async fn responses_stream_tool_calls() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolRouter);
    let filter = responses_filter(table, router);

    let body = serde_json::json!({
        "model": "gpt-4o",
        "input": "What's the weather?",
        "stream": true,
        "tools": [{
            "type": "function",
            "name": "get_weather",
            "parameters": {}
        }]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/responses")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    let events = parse_sse_body(res.body());
    // ToolCall produces: output_item.added, arguments.delta, arguments.done, output_item.done
    // Then Finish produces: response.completed
    // Then [DONE]
    assert!(events.len() >= 5);

    let event_types: Vec<Option<&str>> = events.iter().map(|(e, _)| e.as_deref()).collect();
    assert!(event_types.contains(&Some("response.output_item.added")));
    assert!(event_types.contains(&Some("response.function_call_arguments.delta")));
    assert!(event_types.contains(&Some("response.function_call_arguments.done")));
    assert!(event_types.contains(&Some("response.output_item.done")));
    assert!(event_types.contains(&Some("response.completed")));

    // Check the `arguments.done` event has the full arguments
    let done_event = events
        .iter()
        .find(|(e, _)| e.as_deref() == Some("response.function_call_arguments.done"))
        .unwrap();
    let done_json: Value = serde_json::from_str(&done_event.1).unwrap();
    assert_eq!(done_json["arguments"], r#"{"location":"Paris"}"#);
    assert_eq!(done_json["call_id"], "call_abc123");
}
