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
use serde_json::Value;
use std::collections::HashMap;

use super::filters::generate_content_filter;

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
