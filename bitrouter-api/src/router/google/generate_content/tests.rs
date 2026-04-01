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
        routing_table::{ApiProtocol, RoutingTable, RoutingTarget},
    },
};
use regex::Regex;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::{AtomicU64, Ordering};
use warp::Filter;

use super::filters::{generate_content_filter, generate_content_filter_with_observe};

// ── Mock implementations ────────────────────────────────────────────────────

struct MockTable;
impl RoutingTable for MockTable {
    async fn route(&self, incoming: &str) -> Result<RoutingTarget> {
        Ok(RoutingTarget {
            provider_name: "mock".to_owned(),
            service_id: incoming.to_owned(),
            api_protocol: ApiProtocol::Google,
        })
    }
}

struct MockRouter;
impl LanguageModelRouter for MockRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(MockModel {
            model_id: target.service_id,
        }))
    }
}

struct MockToolRouter;
impl LanguageModelRouter for MockToolRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(MockToolModel {
            model_id: target.service_id,
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
                text: "Hello from Google mock!".to_owned(),
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
                tool_call_id: "call_abc123".to_owned(),
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
                tool_call_id: "call_abc123".to_owned(),
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
async fn generate_content() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = generate_content_filter(table, router);

    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "Hello"}]}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:generateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["candidates"][0]["content"]["role"], "model");
    assert_eq!(
        json["candidates"][0]["content"]["parts"][0]["text"],
        "Hello from Google mock!"
    );
    assert_eq!(json["candidates"][0]["finishReason"], "STOP");
    assert_eq!(json["usageMetadata"]["promptTokenCount"], 12);
    assert_eq!(json["usageMetadata"]["candidatesTokenCount"], 6);
    assert_eq!(json["usageMetadata"]["totalTokenCount"], 18);
    assert_eq!(json["modelVersion"], "gemini-2.0-flash");
}

#[tokio::test]
async fn generate_content_with_system() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = generate_content_filter(table, router);

    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "Hello"}]}
        ],
        "systemInstruction": {
            "parts": [{"text": "You are a helpful assistant."}]
        },
        "generationConfig": {
            "temperature": 0.7,
            "maxOutputTokens": 512
        }
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:generateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
}

#[tokio::test]
async fn generate_content_streaming_sends_sse_events() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = generate_content_filter(table, router);

    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "Hello"}]}
        ],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:streamGenerateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");

    let events = parse_sse_body(res.body());
    assert_eq!(events.len(), 3);

    // First event: text delta
    let delta: Value = serde_json::from_str(&events[0].1).unwrap();
    assert_eq!(delta["candidates"][0]["content"]["role"], "model");
    assert_eq!(
        delta["candidates"][0]["content"]["parts"][0]["text"],
        "Hello"
    );

    // Second event: finish with usage
    let finish: Value = serde_json::from_str(&events[1].1).unwrap();
    assert_eq!(finish["candidates"][0]["finishReason"], "STOP");
    assert_eq!(finish["usageMetadata"]["promptTokenCount"], 12);
    assert_eq!(finish["usageMetadata"]["candidatesTokenCount"], 6);
    assert_eq!(finish["modelVersion"], "gemini-2.0-flash");

    // Third event: [DONE]
    assert_eq!(events[2].1, "[DONE]");
}

#[tokio::test]
async fn generate_content_wrong_method() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = generate_content_filter(table, router);

    let res = warp::test::request()
        .method("GET")
        .path("/v1beta/models/gemini-2.0-flash:generateContent")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 405);
}

#[tokio::test]
async fn generate_content_stream_via_path() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = generate_content_filter(table, router);

    // Stream indicated via path (:streamGenerateContent) without stream field in body
    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "Hello"}]}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:streamGenerateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    assert_eq!(res.headers()["content-type"], "text/event-stream");
}

#[tokio::test]
async fn generate_content_with_tools() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolRouter);
    let filter = generate_content_filter(table, router);

    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "What's the weather in NYC?"}]}
        ],
        "tools": [{
            "functionDeclarations": [{
                "name": "get_weather",
                "description": "Get weather for a location",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "location": {"type": "string"}
                    },
                    "required": ["location"]
                }
            }]
        }]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:generateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    let part = &json["candidates"][0]["content"]["parts"][0];
    assert_eq!(part["functionCall"]["name"], "get_weather");
    assert_eq!(part["functionCall"]["args"]["location"], "NYC");
}

#[tokio::test]
async fn generate_content_stream_tool_calls() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolRouter);
    let filter = generate_content_filter(table, router);

    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "Weather?"}]}
        ],
        "tools": [{"functionDeclarations": [{"name": "get_weather", "parameters": {}}]}],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:streamGenerateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    let events = parse_sse_body(res.body());
    let mut found_function_call = false;
    for (_, data) in &events {
        if data == "[DONE]" {
            continue;
        }
        let json: Value = serde_json::from_str(data).unwrap();
        if let Some(parts) = json["candidates"][0]["content"]["parts"].as_array() {
            for part in parts {
                if part.get("functionCall").is_some() {
                    assert_eq!(part["functionCall"]["name"], "get_weather");
                    assert_eq!(part["functionCall"]["args"]["location"], "NYC");
                    found_function_call = true;
                }
            }
        }
    }
    assert!(
        found_function_call,
        "expected a functionCall part in stream"
    );
}

#[tokio::test]
async fn generate_content_function_response_multi_turn() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = generate_content_filter(table, router);

    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "What's the weather?"}]},
            {"role": "model", "parts": [{"functionCall": {"name": "get_weather", "args": {"location": "NYC"}}}]},
            {"role": "user", "parts": [{"functionResponse": {"name": "get_weather", "response": {"content": "72°F"}}}]}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:generateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["candidates"][0]["content"]["role"], "model");
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
async fn generate_content_observe_success() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let observer = Arc::new(MockObserver::new());
    let filter = generate_content_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "Hello"}]}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:generateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(observer.success_count.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn generate_content_observe_streaming_success() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let observer = Arc::new(MockObserver::new());
    let filter = generate_content_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "gemini-2.0-flash",
        "contents": [
            {"role": "user", "parts": [{"text": "Hello"}]}
        ],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1beta/models/gemini-2.0-flash:streamGenerateContent")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(observer.success_count.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure_count.load(Ordering::SeqCst), 0);
}
