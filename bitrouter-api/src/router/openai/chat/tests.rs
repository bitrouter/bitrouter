use std::sync::Arc;

use bitrouter_core::{
    errors::{BitrouterError, ProviderErrorContext, Result},
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
        content::RouteContext,
        router::LanguageModelRouter,
        routing_table::{ApiProtocol, RoutingTable, RoutingTarget},
    },
};
use regex::Regex;
use std::collections::HashMap;
use std::sync::Mutex;
use std::sync::atomic::{AtomicU64, Ordering};
use warp::Filter;

use super::filters::{chat_completions_filter, chat_completions_filter_with_observe};

// ── Mock implementations ────────────────────────────────────────────────────

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

struct MockTable;
impl RoutingTable for MockTable {
    async fn route(&self, incoming: &str, _context: &RouteContext) -> Result<RoutingTarget> {
        Ok(RoutingTarget {
            provider_name: "mock".to_owned(),
            service_id: incoming.to_owned(),
            api_protocol: ApiProtocol::Openai,
            api_key_override: None,
            api_base_override: None,
            preset: None,
        })
    }
}

struct ChainTable;
impl RoutingTable for ChainTable {
    async fn route(&self, incoming: &str, _context: &RouteContext) -> Result<RoutingTarget> {
        Ok(target("primary", incoming))
    }

    async fn route_chain(
        &self,
        incoming: &str,
        _context: &RouteContext,
    ) -> Result<Vec<RoutingTarget>> {
        Ok(vec![
            target("primary", incoming),
            target("fallback", incoming),
        ])
    }
}

fn target(provider: &str, service_id: &str) -> RoutingTarget {
    RoutingTarget {
        provider_name: provider.to_owned(),
        service_id: service_id.to_owned(),
        api_protocol: ApiProtocol::Openai,
        api_key_override: None,
        api_base_override: None,
        preset: None,
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

struct FailoverRouter;
impl LanguageModelRouter for FailoverRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(FailoverModel {
            provider_name: target.provider_name,
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

struct MockToolStreamRouter;
impl LanguageModelRouter for MockToolStreamRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(MockToolStreamModel {
            model_id: target.service_id,
        }))
    }
}

#[derive(Clone)]
struct MockModel {
    model_id: String,
}

#[derive(Clone)]
struct FailoverModel {
    provider_name: String,
    model_id: String,
}

impl LanguageModel for FailoverModel {
    fn provider_name(&self) -> &str {
        &self.provider_name
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
        if self.provider_name == "primary" {
            return Err(BitrouterError::provider_error(
                "primary",
                "temporary upstream failure",
                ProviderErrorContext {
                    status_code: Some(500),
                    error_type: None,
                    code: None,
                    param: None,
                    request_id: None,
                    body: None,
                },
            ));
        }
        Ok(LanguageModelGenerateResult {
            content: vec![LanguageModelContent::Text {
                text: "Hello from fallback!".to_owned(),
                provider_metadata: None,
            }],
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
        if self.provider_name == "primary" {
            return Err(BitrouterError::provider_error(
                "primary",
                "temporary upstream failure",
                ProviderErrorContext {
                    status_code: Some(500),
                    error_type: None,
                    code: None,
                    param: None,
                    request_id: None,
                    body: None,
                },
            ));
        }
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let _ = tx
            .send(LanguageModelStreamPart::TextDelta {
                id: "0".to_owned(),
                delta: "fallback".to_owned(),
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::Finish {
                usage: mock_usage(),
                finish_reason: LanguageModelFinishReason::Stop,
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
            content: vec![LanguageModelContent::Text {
                text: "Hello from mock model!".to_owned(),
                provider_metadata: None,
            }],
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
            .send(LanguageModelStreamPart::TextDelta {
                id: "0".to_owned(),
                delta: "Hello".to_owned(),
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::Finish {
                usage: mock_usage(),
                finish_reason: LanguageModelFinishReason::Stop,
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

/// Mock model that returns a tool call result.
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
            content: vec![LanguageModelContent::ToolCall {
                tool_call_id: "call_abc123".to_owned(),
                tool_name: "write_file".to_owned(),
                tool_input: r#"{"path":"test.txt","content":"hello"}"#.to_owned(),
                provider_executed: None,
                dynamic: None,
                provider_metadata: None,
            }],
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
                tool_name: "write_file".to_owned(),
                tool_input: r#"{"path":"test.txt","content":"hello"}"#.to_owned(),
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

/// Mock model that streams tool call deltas (ToolInputStart/Delta/End).
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
            content: vec![LanguageModelContent::Text {
                text: String::new(),
                provider_metadata: None,
            }],
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
                id: "call_xyz".to_owned(),
                tool_name: "read_file".to_owned(),
                provider_executed: None,
                dynamic: None,
                title: None,
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::ToolInputDelta {
                id: "call_xyz".to_owned(),
                delta: r#"{"path":"#.to_owned(),
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::ToolInputDelta {
                id: "call_xyz".to_owned(),
                delta: r#""foo.txt"}"#.to_owned(),
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::ToolInputEnd {
                id: "call_xyz".to_owned(),
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

struct AllFailChainTable;
impl RoutingTable for AllFailChainTable {
    async fn route(&self, incoming: &str, _context: &RouteContext) -> Result<RoutingTarget> {
        Ok(target("primary", incoming))
    }

    async fn route_chain(
        &self,
        incoming: &str,
        _context: &RouteContext,
    ) -> Result<Vec<RoutingTarget>> {
        Ok(vec![
            target("primary", incoming),
            target("fallback", incoming),
        ])
    }
}

struct AlwaysRetryableErrorModel;

impl LanguageModel for AlwaysRetryableErrorModel {
    fn provider_name(&self) -> &str {
        "always-fail"
    }
    fn model_id(&self) -> &str {
        "always-fail-model"
    }
    async fn supported_urls(&self) -> HashMap<String, Regex> {
        HashMap::new()
    }
    async fn generate(
        &self,
        _options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        Err(BitrouterError::provider_error(
            "always-fail",
            "upstream unavailable",
            ProviderErrorContext {
                status_code: Some(500),
                error_type: None,
                code: None,
                param: None,
                request_id: None,
                body: None,
            },
        ))
    }
    async fn stream(
        &self,
        _options: LanguageModelCallOptions,
    ) -> Result<LanguageModelStreamResult> {
        Err(BitrouterError::provider_error(
            "always-fail",
            "upstream unavailable",
            ProviderErrorContext {
                status_code: Some(500),
                error_type: None,
                code: None,
                param: None,
                request_id: None,
                body: None,
            },
        ))
    }
}

struct AlwaysRetryableRouter;
impl LanguageModelRouter for AlwaysRetryableRouter {
    async fn route_model(&self, _target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(AlwaysRetryableErrorModel))
    }
}

struct AuthFailChainTable;
impl RoutingTable for AuthFailChainTable {
    async fn route(&self, incoming: &str, _context: &RouteContext) -> Result<RoutingTarget> {
        Ok(target("denied", incoming))
    }

    async fn route_chain(
        &self,
        incoming: &str,
        _context: &RouteContext,
    ) -> Result<Vec<RoutingTarget>> {
        Ok(vec![
            target("denied", incoming),
            target("fallback", incoming),
        ])
    }
}

#[derive(Clone)]
struct AuthFailThenSucceedModel {
    provider_name: String,
}

impl LanguageModel for AuthFailThenSucceedModel {
    fn provider_name(&self) -> &str {
        &self.provider_name
    }
    fn model_id(&self) -> &str {
        "test-model"
    }
    async fn supported_urls(&self) -> HashMap<String, Regex> {
        HashMap::new()
    }
    async fn generate(
        &self,
        _options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        if self.provider_name == "denied" {
            return Err(BitrouterError::AccessDenied {
                message: "invalid API key".to_owned(),
            });
        }
        Ok(LanguageModelGenerateResult {
            content: vec![LanguageModelContent::Text {
                text: "Hello from fallback!".to_owned(),
                provider_metadata: None,
            }],
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
        if self.provider_name == "denied" {
            return Err(BitrouterError::AccessDenied {
                message: "invalid API key".to_owned(),
            });
        }
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let _ = tx
            .send(LanguageModelStreamPart::TextDelta {
                id: "0".to_owned(),
                delta: "fallback".to_owned(),
                provider_metadata: None,
            })
            .await;
        let _ = tx
            .send(LanguageModelStreamPart::Finish {
                usage: mock_usage(),
                finish_reason: LanguageModelFinishReason::Stop,
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

struct AuthFailRouter;
impl LanguageModelRouter for AuthFailRouter {
    async fn route_model(&self, target: RoutingTarget) -> Result<Box<DynLanguageModel<'static>>> {
        Ok(DynLanguageModel::new_box(AuthFailThenSucceedModel {
            provider_name: target.provider_name,
        }))
    }
}

// ── Tests ───────────────────────────────────────────────────────────────────

#[tokio::test]
async fn chat_completions_generate() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["object"], "chat.completion");
    assert_eq!(json["model"], "test-model");
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "Hello from mock model!"
    );
    assert_eq!(json["choices"][0]["finish_reason"], "stop");
    assert!(json["usage"]["prompt_tokens"].as_u64().unwrap() > 0);
}

#[tokio::test]
async fn chat_completions_falls_back_on_retryable_generate_error() {
    let table = Arc::new(ChainTable);
    let router = Arc::new(FailoverRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(
        json["choices"][0]["message"]["content"],
        "Hello from fallback!"
    );
}

#[tokio::test]
async fn chat_completions_falls_back_on_stream_init_error() {
    let table = Arc::new(ChainTable);
    let router = Arc::new(FailoverRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello"}
        ],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let body_str = String::from_utf8_lossy(res.body());
    assert!(body_str.contains("fallback"));
}

#[tokio::test]
async fn chat_completions_wrong_method() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = chat_completions_filter(table, router);

    let res = warp::test::request()
        .method("GET")
        .path("/v1/chat/completions")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 405);
}

#[tokio::test]
async fn chat_completions_wrong_path() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = chat_completions_filter(table, router);

    let res = warp::test::request()
        .method("POST")
        .path("/v1/other")
        .json(&serde_json::json!({"model": "x", "messages": []}))
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 404);
}

#[tokio::test]
async fn chat_completions_system_and_user() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "system", "content": "You are helpful."},
            {"role": "user", "content": "Hello"}
        ],
        "temperature": 0.5,
        "max_completion_tokens": 100
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["choices"][0]["message"]["role"], "assistant");
}

#[tokio::test]
async fn chat_completions_with_tools() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Write hello to test.txt"}
        ],
        "tools": [{
            "type": "function",
            "function": {
                "name": "write_file",
                "description": "Write to file",
                "parameters": {
                    "type": "object",
                    "properties": {
                        "path": {"type": "string"},
                        "content": {"type": "string"}
                    },
                    "required": ["path", "content"]
                }
            }
        }],
        "tool_choice": "auto"
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["choices"][0]["finish_reason"], "tool_calls");
    assert!(json["choices"][0]["message"]["content"].is_null());

    let tool_calls = &json["choices"][0]["message"]["tool_calls"];
    assert_eq!(tool_calls[0]["id"], "call_abc123");
    assert_eq!(tool_calls[0]["type"], "function");
    assert_eq!(tool_calls[0]["function"]["name"], "write_file");

    let args: serde_json::Value =
        serde_json::from_str(tool_calls[0]["function"]["arguments"].as_str().unwrap()).unwrap();
    assert_eq!(args["path"], "test.txt");
    assert_eq!(args["content"], "hello");
}

#[tokio::test]
async fn chat_completions_tool_multi_turn() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Write hello to test.txt"},
            {
                "role": "assistant",
                "content": null,
                "tool_calls": [{
                    "id": "call_abc",
                    "type": "function",
                    "function": {
                        "name": "write_file",
                        "arguments": "{\"path\":\"test.txt\",\"content\":\"hello\"}"
                    }
                }]
            },
            {
                "role": "tool",
                "tool_call_id": "call_abc",
                "name": "write_file",
                "content": "File written successfully"
            },
            {"role": "user", "content": "Thanks!"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);
    let json: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
    assert_eq!(json["choices"][0]["message"]["role"], "assistant");
}

#[tokio::test]
async fn chat_completions_stream_tool_calls() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "Write hello"}],
        "tools": [{"type": "function", "function": {"name": "write_file", "parameters": {}}}],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    let body_str = String::from_utf8_lossy(res.body());
    let events: Vec<&str> = body_str
        .lines()
        .filter(|line| line.starts_with("data:"))
        .map(|line| line.trim_start_matches("data:").trim())
        .collect();

    // Should have a tool_calls chunk and a finish chunk and [DONE]
    let mut found_tool_call = false;
    let mut found_finish = false;
    for event in &events {
        if *event == "[DONE]" {
            continue;
        }
        let json: serde_json::Value = serde_json::from_str(event).unwrap();
        if let Some(tool_calls) = json["choices"][0]["delta"]["tool_calls"].as_array() {
            assert_eq!(tool_calls[0]["function"]["name"], "write_file");
            found_tool_call = true;
        }
        if json["choices"][0]["finish_reason"].as_str() == Some("tool_calls") {
            found_finish = true;
        }
    }
    assert!(found_tool_call, "expected a tool_calls delta chunk");
    assert!(found_finish, "expected finish_reason tool_calls");
}

#[tokio::test]
async fn chat_completions_stream_tool_input_deltas() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockToolStreamRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "Read foo.txt"}],
        "tools": [{"type": "function", "function": {"name": "read_file", "parameters": {}}}],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    let body_str = String::from_utf8_lossy(res.body());
    let events: Vec<&str> = body_str
        .lines()
        .filter(|line| line.starts_with("data:"))
        .map(|line| line.trim_start_matches("data:").trim())
        .collect();

    let mut tool_start_seen = false;
    let mut argument_fragments = Vec::new();
    for event in &events {
        if *event == "[DONE]" {
            continue;
        }
        let json: serde_json::Value = serde_json::from_str(event).unwrap();
        if let Some(tool_calls) = json["choices"][0]["delta"]["tool_calls"].as_array() {
            let tc = &tool_calls[0];
            if tc["id"].is_string() {
                // ToolInputStart — has id and function name
                assert_eq!(tc["id"], "call_xyz");
                assert_eq!(tc["type"], "function");
                assert_eq!(tc["function"]["name"], "read_file");
                tool_start_seen = true;
            }
            if let Some(args) = tc["function"]["arguments"].as_str() {
                argument_fragments.push(args.to_owned());
            }
        }
    }
    assert!(tool_start_seen, "expected ToolInputStart chunk");
    let full_args: String = argument_fragments.concat();
    assert_eq!(full_args, r#"{"path":"foo.txt"}"#);
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

struct CapturingObserver {
    success_events: Mutex<Vec<RequestSuccessEvent>>,
    failure_events: Mutex<Vec<RequestFailureEvent>>,
}

impl CapturingObserver {
    fn new() -> Self {
        Self {
            success_events: Mutex::new(Vec::new()),
            failure_events: Mutex::new(Vec::new()),
        }
    }
}

impl ObserveCallback for CapturingObserver {
    fn on_request_success(
        &self,
        event: RequestSuccessEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.success_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
        Box::pin(async {})
    }

    fn on_request_failure(
        &self,
        event: RequestFailureEvent,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = ()> + Send + '_>> {
        self.failure_events
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .push(event);
        Box::pin(async {})
    }
}

#[tokio::test]
async fn chat_completions_all_chain_targets_fail_generate() {
    let table = Arc::new(AllFailChainTable);
    let router = Arc::new(AlwaysRetryableRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert!(!res.status().is_success());
    let body_str = String::from_utf8_lossy(res.body());
    assert!(body_str.contains("upstream unavailable"));
}

#[tokio::test]
async fn chat_completions_all_chain_targets_fail_stream() {
    let table = Arc::new(AllFailChainTable);
    let router = Arc::new(AlwaysRetryableRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello"}
        ],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert!(!res.status().is_success());
    let body_str = String::from_utf8_lossy(res.body());
    assert!(body_str.contains("upstream unavailable"));
}

#[tokio::test]
async fn chat_completions_non_retryable_error_stops_immediately_generate() {
    let table = Arc::new(AuthFailChainTable);
    let router = Arc::new(AuthFailRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello"}
        ]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert!(!res.status().is_success());
    let body_str = String::from_utf8_lossy(res.body());
    assert!(body_str.contains("invalid API key"));
}

#[tokio::test]
async fn chat_completions_non_retryable_error_stops_immediately_stream() {
    let table = Arc::new(AuthFailChainTable);
    let router = Arc::new(AuthFailRouter);
    let filter = chat_completions_filter(table, router);

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [
            {"role": "user", "content": "Hello"}
        ],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert!(!res.status().is_success());
    let body_str = String::from_utf8_lossy(res.body());
    assert!(body_str.contains("invalid API key"));
}

// ── Observe tests ───────────────────────────────────────────────────────

#[tokio::test]
async fn chat_completions_observe_success() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let observer = Arc::new(MockObserver::new());
    let filter = chat_completions_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    // Observer is dispatched via tokio::spawn; give it time to complete.
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert_eq!(observer.success_count.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn chat_completions_observe_streaming_success() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let observer = Arc::new(MockObserver::new());
    let filter = chat_completions_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "Hello"}],
        "stream": true
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    // Streaming observer runs in the spawned task; give it time to complete.
    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    assert_eq!(observer.success_count.load(Ordering::SeqCst), 1);
    assert_eq!(observer.failure_count.load(Ordering::SeqCst), 0);
}

#[tokio::test]
async fn chat_completions_observe_fallback_records_executed_target() {
    let table = Arc::new(ChainTable);
    let router = Arc::new(FailoverRouter);
    let observer = Arc::new(CapturingObserver::new());
    let filter = chat_completions_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 200);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let events = observer
        .success_events
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(events.len(), 1);
    let executed = events[0]
        .executed_target
        .as_ref()
        .expect("executed_target should be set");
    assert_eq!(executed.provider_name, "fallback");
    assert_eq!(executed.service_id, "test-model");
}

#[tokio::test]
async fn chat_completions_observe_all_fail_records_last_target() {
    let table = Arc::new(AllFailChainTable);
    let router = Arc::new(AlwaysRetryableRouter);
    let observer = Arc::new(CapturingObserver::new());
    let filter = chat_completions_filter_with_observe(
        table,
        router,
        observer.clone(),
        warp::any().and_then(|| async { Ok::<_, warp::Rejection>(CallerContext::default()) }),
    );

    let body = serde_json::json!({
        "model": "test-model",
        "messages": [{"role": "user", "content": "Hello"}]
    });

    let res = warp::test::request()
        .method("POST")
        .path("/v1/chat/completions")
        .json(&body)
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 500);

    tokio::time::sleep(std::time::Duration::from_millis(50)).await;

    let events = observer
        .failure_events
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    assert_eq!(events.len(), 1);
    let executed = events[0]
        .executed_target
        .as_ref()
        .expect("executed_target should be set");
    assert_eq!(executed.provider_name, "fallback");
}
