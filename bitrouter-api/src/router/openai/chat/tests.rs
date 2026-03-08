use std::sync::Arc;

use bitrouter_core::{
    errors::Result,
    models::language::{
        call_options::LanguageModelCallOptions,
        content::LanguageModelContent,
        finish_reason::LanguageModelFinishReason,
        generate_result::LanguageModelGenerateResult,
        language_model::{DynLanguageModel, LanguageModel},
        stream_result::LanguageModelStreamResult,
        usage::{LanguageModelInputTokens, LanguageModelOutputTokens, LanguageModelUsage},
    },
    routers::{
        model_router::LanguageModelRouter,
        routing_table::{RoutingTable, RoutingTarget},
    },
};
use regex::Regex;
use std::collections::HashMap;

use super::filters::chat_completions_filter;

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
                text: "Hello from mock model!".to_owned(),
                provider_metadata: None,
            },
            finish_reason: LanguageModelFinishReason::Stop,
            usage: LanguageModelUsage {
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
