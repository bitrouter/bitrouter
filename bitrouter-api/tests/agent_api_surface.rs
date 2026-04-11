//! Integration tests for the HTTP API surfaces used by all supported agents.
//!
//! Each agent talks to BitRouter using one of three API protocols:
//!
//! - **OpenAI Chat Completions** (`POST /v1/chat/completions`)
//!   Used by: codex, openclaw, deepagents, goose, opencode, cline, kilo,
//!   openhands, hermes, pi
//!
//! - **Anthropic Messages** (`POST /v1/messages`)
//!   Used by: claude, openclaw, deepagents, goose, cline
//!
//! - **Google Generative AI** (`POST /v1beta/models/:model_action`)
//!   Used by: gemini
//!
//! These tests verify that each protocol surface accepts well-formed requests,
//! routes them through the mock routing table, and returns valid responses
//! in the expected format — proving the full HTTP pipeline works for every
//! agent configuration.

use std::collections::HashMap;
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
    routers::{
        content::RouteContext,
        router::LanguageModelRouter,
        routing_table::{ApiProtocol, RoutingTable, RoutingTarget},
    },
};
use regex::Regex;
use serde_json::Value;
use warp::Filter;

// ── Mock implementations ──────────────────────────────────────────────

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

/// Mock routing table that accepts any model name and routes to the "mock"
/// provider. This simulates how ConfigRoutingTable resolves
/// `openrouter:anthropic/claude-3.5-haiku` → provider=openrouter, model=...
struct MockTable {
    protocol: ApiProtocol,
}

impl RoutingTable for MockTable {
    async fn route(&self, incoming: &str, _context: &RouteContext) -> Result<RoutingTarget> {
        Ok(RoutingTarget {
            provider_name: "mock".to_owned(),
            service_id: incoming.to_owned(),
            api_protocol: self.protocol,
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
                text: format!("Hello from mock model ({})!", self.model_id),
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
        let model_id = self.model_id.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(8);
        let _ = tx
            .send(LanguageModelStreamPart::TextDelta {
                id: "0".to_owned(),
                delta: format!("Hello from {model_id}"),
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

// ═══════════════════════════════════════════════════════════════════════
// OpenAI Chat Completions API surface
// Used by: codex, openclaw, deepagents, goose, opencode, cline, kilo
// ═══════════════════════════════════════════════════════════════════════

mod openai_chat {
    use super::*;
    use bitrouter_api::router::openai::chat::filters::chat_completions_filter;

    fn filter() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let table = Arc::new(MockTable {
            protocol: ApiProtocol::Openai,
        });
        let router = Arc::new(MockRouter);
        chat_completions_filter(table, router)
    }

    /// Simulates what codex sends: `POST /v1/chat/completions` with
    /// `model: "openrouter:anthropic/claude-3.5-haiku"`.
    #[tokio::test]
    async fn codex_chat_completions() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-3.5-haiku",
                "messages": [{"role": "user", "content": "Hello from codex!"}],
                "max_tokens": 50
            }))
            .reply(&filter())
            .await;

        assert_eq!(
            res.status(),
            200,
            "body: {}",
            String::from_utf8_lossy(res.body())
        );
        let body: Value = serde_json::from_slice(res.body()).expect("valid JSON");
        assert_eq!(body["object"], "chat.completion");
        assert!(
            body["choices"][0]["message"]["content"]
                .as_str()
                .is_some_and(|s| s.contains("mock model"))
        );
    }

    /// Simulates what openclaw sends via OpenAI format.
    #[tokio::test]
    async fn openclaw_chat_completions() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "openrouter:openai/gpt-4o",
                "messages": [
                    {"role": "system", "content": "You are a helpful assistant."},
                    {"role": "user", "content": "Hello from openclaw!"}
                ],
                "max_tokens": 100,
                "temperature": 0.7
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
        let body: Value = serde_json::from_slice(res.body()).expect("valid JSON");
        assert_eq!(body["object"], "chat.completion");
    }

    /// Simulates what deepagents sends via OpenAI format.
    #[tokio::test]
    async fn deepagents_chat_completions() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-opus-4",
                "messages": [{"role": "user", "content": "Hello from deepagents!"}],
                "max_tokens": 200
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
        let body: Value = serde_json::from_slice(res.body()).expect("valid JSON");
        assert!(body["choices"][0]["message"]["content"].as_str().is_some());
    }

    /// Simulates what goose sends (OPENAI_HOST route).
    #[tokio::test]
    async fn goose_chat_completions() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "openrouter:openai/gpt-4.1-mini",
                "messages": [{"role": "user", "content": "Hello from goose!"}],
                "max_tokens": 50
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
        let body: Value = serde_json::from_slice(res.body()).expect("valid JSON");
        assert_eq!(body["object"], "chat.completion");
    }

    /// Simulates what opencode sends (LOCAL_ENDPOINT route).
    #[tokio::test]
    async fn opencode_chat_completions() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-sonnet-4",
                "messages": [{"role": "user", "content": "Hello from opencode!"}],
                "max_tokens": 100
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
    }

    /// Simulates what cline sends via OpenAI format.
    #[tokio::test]
    async fn cline_chat_completions() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-3.5-haiku",
                "messages": [{"role": "user", "content": "Hello from cline!"}],
                "max_tokens": 50
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
    }

    /// Tests streaming response format (used by most agents).
    /// warp::test collects the full SSE body; we verify a 200 response
    /// which confirms the handler accepted the stream=true flag.
    #[tokio::test]
    async fn streaming_chat_completions() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/chat/completions")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-3.5-haiku",
                "messages": [{"role": "user", "content": "Stream test"}],
                "stream": true,
                "max_tokens": 50
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Anthropic Messages API surface
// Used by: claude, openclaw (dual), deepagents (dual), goose (dual), cline
// ═══════════════════════════════════════════════════════════════════════

mod anthropic_messages {
    use super::*;
    use bitrouter_api::router::anthropic::messages::filters::messages_filter;

    fn filter() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let table = Arc::new(MockTable {
            protocol: ApiProtocol::Anthropic,
        });
        let router = Arc::new(MockRouter);
        messages_filter(table, router)
    }

    /// Simulates what claude sends: `POST /v1/messages` with Anthropic format.
    /// Claude sets ANTHROPIC_BASE_URL to bitrouter/v1 and sends native format.
    #[tokio::test]
    async fn claude_messages() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", "sk-ant-test")
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-opus-4",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "Hello from claude agent!"}]
            }))
            .reply(&filter())
            .await;

        assert_eq!(
            res.status(),
            200,
            "body: {}",
            String::from_utf8_lossy(res.body())
        );
        let body: Value = serde_json::from_slice(res.body()).expect("valid JSON");
        // Anthropic Messages API returns type: "message"
        assert_eq!(body["type"], "message");
        assert_eq!(body["role"], "assistant");
        assert!(
            body["content"][0]["text"]
                .as_str()
                .is_some_and(|s| s.contains("mock model"))
        );
    }

    /// Simulates what openclaw sends via Anthropic format (dual-protocol agent).
    #[tokio::test]
    async fn openclaw_anthropic_messages() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", "sk-ant-test")
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-sonnet-4",
                "max_tokens": 200,
                "messages": [{"role": "user", "content": "Hello from openclaw (anthropic)!"}]
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
        let body: Value = serde_json::from_slice(res.body()).expect("valid JSON");
        assert_eq!(body["type"], "message");
    }

    /// Simulates what deepagents sends via Anthropic format.
    #[tokio::test]
    async fn deepagents_anthropic_messages() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", "sk-ant-test")
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-opus-4",
                "max_tokens": 300,
                "messages": [{"role": "user", "content": "Hello from deepagents (anthropic)!"}]
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
    }

    /// Simulates what goose sends via Anthropic format (ANTHROPIC_HOST route).
    #[tokio::test]
    async fn goose_anthropic_messages() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", "sk-ant-test")
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-3.7-sonnet",
                "max_tokens": 100,
                "messages": [{"role": "user", "content": "Hello from goose (anthropic)!"}]
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
        let body: Value = serde_json::from_slice(res.body()).expect("valid JSON");
        assert_eq!(body["type"], "message");
    }

    /// Tests Anthropic streaming format (SSE with event types).
    /// A 200 confirms the handler accepted stream=true and produced a response.
    #[tokio::test]
    async fn claude_streaming_messages() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1/messages")
            .header("content-type", "application/json")
            .header("x-api-key", "sk-ant-test")
            .header("anthropic-version", "2023-06-01")
            .json(&serde_json::json!({
                "model": "openrouter:anthropic/claude-opus-4",
                "max_tokens": 100,
                "stream": true,
                "messages": [{"role": "user", "content": "Stream test from claude"}]
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
    }
}

// ═══════════════════════════════════════════════════════════════════════
// Google Generative AI API surface
// Used by: gemini
// ═══════════════════════════════════════════════════════════════════════

mod google_generative_ai {
    use super::*;
    use bitrouter_api::router::google::generate_content::filters::generate_content_filter;

    fn filter() -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
        let table = Arc::new(MockTable {
            protocol: ApiProtocol::Google,
        });
        let router = Arc::new(MockRouter);
        generate_content_filter(table, router)
    }

    /// Simulates what gemini sends: Google Generative AI format.
    /// gemini CLI uses `POST /v1beta/models/<model>:generateContent`.
    /// Note: model names with `/` are url-encoded as path segments.
    #[tokio::test]
    async fn gemini_generate_content() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1beta/models/gemini-2.5-flash:generateContent")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "contents": [{
                    "role": "user",
                    "parts": [{"text": "Hello from gemini!"}]
                }],
                "generationConfig": {
                    "maxOutputTokens": 50
                }
            }))
            .reply(&filter())
            .await;

        assert_eq!(
            res.status(),
            200,
            "body: {}",
            String::from_utf8_lossy(res.body())
        );
        let body: Value = serde_json::from_slice(res.body()).expect("valid JSON");
        // Google API returns candidates array
        assert!(
            body["candidates"][0]["content"]["parts"][0]["text"]
                .as_str()
                .is_some_and(|s| s.contains("mock model"))
        );
    }

    /// Tests Google streaming format.
    /// A 200 confirms the handler accepted streamGenerateContent.
    #[tokio::test]
    async fn gemini_stream_generate_content() {
        let res = warp::test::request()
            .method("POST")
            .path("/v1beta/models/gemini-2.5-flash:streamGenerateContent")
            .header("content-type", "application/json")
            .json(&serde_json::json!({
                "contents": [{
                    "role": "user",
                    "parts": [{"text": "Stream test from gemini"}]
                }],
                "generationConfig": {
                    "maxOutputTokens": 50
                }
            }))
            .reply(&filter())
            .await;

        assert_eq!(res.status(), 200);
    }
}
