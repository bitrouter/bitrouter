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
        model_router::LanguageModelRouter,
        routing_table::{RoutingTable, RoutingTarget},
    },
};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use warp::Filter;

use super::filters::{messages_filter, messages_filter_with_observe};

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

struct MockToolRouter;
impl LanguageModelRouter for MockToolRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(MockToolModel {
            model_id: target.model_id,
        }))
    }
}

struct MockToolStreamRouter;
impl LanguageModelRouter for MockToolStreamRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(MockToolStreamModel {
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
                text: "Hello from Anthropic mock!".to_owned(),
                provider_metadata: None,
            },
            finish_reason: LanguageModelFinishReason::Stop,
            usage: LanguageModelUsage {
                input_tokens: LanguageModelInputTokens {
                    total: Some(12),
                    no_cache: None,
                    cache_read: None,
                    cache_write: None,
                },
                output_tokens: LanguageModelOutputTokens {
                    total: Some(6),
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
                            total: Some(12),
                            no_cache: None,
                            cache_read: None,
                            cache_write: None,
                        },
                        output_tokens: LanguageModelOutputTokens {
                            total: Some(6),
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

fn mock_usage() -> LanguageModelUsage {
    LanguageModelUsage {
        input_tokens: LanguageModelInputTokens {
            total: Some(12),
            no_cache: None,
            cache_read: None,
            cache_write: None,
        },
        output_tokens: LanguageModelOutputTokens {
            total: Some(6),
            text: None,
            reasoning: None,
        },
        raw: None,
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
                tool_call_id: "toolu_abc123".to_owned(),
                tool_name: "get_weather".to_owned(),
                tool_input: r#"{"location":"NYC"}"#.to_owned(),
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
                tool_call_id: "toolu_abc123".to_owned(),
                tool_name: "get_weather".to_owned(),
                tool_input: r#"{"location":"NYC"}"#.to_owned(),
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

#[derive(Clone)]
struct MockToolStreamModel {
    model_id: String,
}

impl LanguageModel for MockToolStreamModel {
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
                text: String::new(),
                provider_metadata: None,
            },
            finish_reason: LanguageModelFinishReason::Stop,
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
            .send(LanguageModelStreamPart::ToolInputStart {
                id: "toolu_xyz".to_owned(),
                tool_name: "get_weather".to_owned(),
                provider_executed: None,
                dynamic: None,
                title: None,
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::ToolInputDelta {
                id: "toolu_xyz".to_owned(),
                delta: r#"{"location":"#.to_owned(),
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::ToolInputDelta {
                id: "toolu_xyz".to_owned(),
                delta: r#""NYC"}"#.to_owned(),
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::ToolInputEnd {
                id: "toolu_xyz".to_owned(),
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
async fn messages_generate() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = messages_filter(table, router);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["type"], "message");
    assert_eq!(json["role"], "assistant");
    assert_eq!(json["model"], "claude-3-5-sonnet-20241022");
    assert_eq!(json["content"][0]["text"], "Hello from Anthropic mock!");
    assert_eq!(json["stop_reason"], "end_turn");
    assert_eq!(json["usage"]["input_tokens"], 12);
    assert_eq!(json["usage"]["output_tokens"], 6);
}

#[tokio::test]
async fn messages_with_system() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = messages_filter(table, router);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 512,
        "system": "You are a helpful assistant.",
        "messages": [
            {"role": "user", "content": "Hello"}
        ],
        "temperature": 0.7
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn messages_streaming_sends_sse_events() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = messages_filter(table, router);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");

    let events = parse_sse_body(res.body());
    assert_eq!(events.len(), 3);

    assert_eq!(events[0].0.as_deref(), Some("content_block_delta"));
    let delta: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(delta["type"], "content_block_delta");
    assert_eq!(delta["index"], 0);
    assert_eq!(delta["delta"]["type"], "text_delta");
    assert_eq!(delta["delta"]["text"], "Hello");

    assert_eq!(events[1].0.as_deref(), Some("message_delta"));
    let finish: Value = serde_json::from_str(&events[1].1).unwrap();
    assert_eq!(finish["type"], "message_delta");
    assert_eq!(finish["delta"]["type"], "message_delta");
    assert_eq!(finish["delta"]["stop_reason"], "end_turn");
    assert_eq!(finish["message"]["model"], "claude-3-5-sonnet-20241022");
    assert_eq!(finish["message"]["role"], "assistant");
    assert!(
        finish["message"]["id"]
            .as_str()
            .unwrap()
            .starts_with("msg-")
    );

    assert_eq!(events[2].0, None);
    assert_eq!(events[2].1, "[DONE]");
}

#[tokio::test]
async fn messages_wrong_method() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = messages_filter(table, router);

    let res = warp::test::request()
        .method("GET")
        .path("/v1/messages")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 405);
}

#[tokio::test]
async fn messages_with_tools() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolRouter);
    let filter = messages_filter(table, router);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "What's the weather in NYC?"}
        ],
        "tools": [{
            "name": "get_weather",
            "description": "Get weather for a location",
            "input_schema": {
                "type": "object",
                "properties": {
                    "location": {"type": "string"}
                },
                "required": ["location"]
            }
        }],
        "tool_choice": {"type": "auto"}
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["stop_reason"], "tool_use");
    assert_eq!(json["content"][0]["type"], "tool_use");
    assert_eq!(json["content"][0]["id"], "toolu_abc123");
    assert_eq!(json["content"][0]["name"], "get_weather");
    assert_eq!(json["content"][0]["input"]["location"], "NYC");
}

#[tokio::test]
async fn messages_tool_result_multi_turn() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = messages_filter(table, router);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "messages": [
            {"role": "user", "content": "What's the weather in NYC?"},
            {
                "role": "assistant",
                "content": [
                    {"type": "tool_use", "id": "toolu_abc", "name": "get_weather", "input": {"location": "NYC"}}
                ]
            },
            {
                "role": "user",
                "content": [
                    {"type": "tool_result", "tool_use_id": "toolu_abc", "content": "72°F and sunny"}
                ]
            }
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["role"], "assistant");
}

#[tokio::test]
async fn messages_stream_tool_calls() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolRouter);
    let filter = messages_filter(table, router);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "messages": [{"role": "user", "content": "Weather?"}],
        "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    let events = parse_sse_body(res.body());
    let mut found_tool_start = false;
    let mut found_tool_delta = false;
    let mut found_tool_stop = false;
    let mut found_finish = false;
    for (event_name, data) in &events {
        if data == "[DONE]" {
            continue;
        }
        let json: Value = serde_json::from_str(data).unwrap();
        if event_name.as_deref() == Some("content_block_start")
            && json["content_block"]["type"] == "tool_use"
        {
            assert_eq!(json["content_block"]["id"], "toolu_abc123");
            assert_eq!(json["content_block"]["name"], "get_weather");
            found_tool_start = true;
        }
        if event_name.as_deref() == Some("content_block_delta")
            && json["delta"]["type"] == "input_json_delta"
        {
            found_tool_delta = true;
        }
        if event_name.as_deref() == Some("content_block_stop") {
            found_tool_stop = true;
        }
        if event_name.as_deref() == Some("message_delta")
            && json["delta"]["stop_reason"] == "tool_use"
        {
            found_finish = true;
        }
    }
    assert!(
        found_tool_start,
        "expected content_block_start for tool_use"
    );
    assert!(
        found_tool_delta,
        "expected content_block_delta with input_json_delta"
    );
    assert!(found_tool_stop, "expected content_block_stop");
    assert!(
        found_finish,
        "expected message_delta with stop_reason tool_use"
    );
}

#[tokio::test]
async fn messages_stream_tool_input_deltas() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolStreamRouter);
    let filter = messages_filter(table, router);

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "messages": [{"role": "user", "content": "Weather?"}],
        "tools": [{"name": "get_weather", "input_schema": {"type": "object"}}]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    let events = parse_sse_body(res.body());
    let mut tool_start_seen = false;
    let mut partial_json_fragments = Vec::new();
    let mut tool_stop_seen = false;
    for (event_name, data) in &events {
        if data == "[DONE]" {
            continue;
        }
        let json: Value = serde_json::from_str(data).unwrap();
        if event_name.as_deref() == Some("content_block_start")
            && json["content_block"]["type"] == "tool_use"
        {
            assert_eq!(json["content_block"]["id"], "toolu_xyz");
            assert_eq!(json["content_block"]["name"], "get_weather");
            tool_start_seen = true;
        }
        if event_name.as_deref() == Some("content_block_delta")
            && json["delta"]["type"] == "input_json_delta"
        {
            if let Some(pj) = json["delta"]["partial_json"].as_str() {
                partial_json_fragments.push(pj.to_owned());
            }
        }
        if event_name.as_deref() == Some("content_block_stop") {
            tool_stop_seen = true;
        }
    }
    assert!(tool_start_seen, "expected content_block_start for tool_use");
    assert!(tool_stop_seen, "expected content_block_stop");
    let full_json: String = partial_json_fragments.concat();
    assert_eq!(full_json, r#"{"location":"NYC"}"#);
}

#[tokio::test]
async fn messages_missing_max_tokens() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = messages_filter(table, router);

    // Anthropic requires max_tokens
    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    // Should fail because max_tokens is required in our type
    assert_ne!(res.status(), 200);
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
async fn messages_observe_success() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let observer = Arc::new(MockObserver::new());
    let filter = messages_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(observer.success_count.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn messages_observe_streaming_success() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let observer = Arc::new(MockObserver::new());
    let filter = messages_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "claude-3-5-sonnet-20241022",
        "max_tokens": 1024,
        "stream": true,
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/messages")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(observer.success_count.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure_count.load(Ordering::SeqCst), 0);
}
