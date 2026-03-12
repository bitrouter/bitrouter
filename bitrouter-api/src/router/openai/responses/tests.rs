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

use super::filters::responses_filter;

use crate::metrics::MetricsStore;

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
    let metrics = Arc::new(MetricsStore::new());
    let filter = responses_filter(table, router, metrics.clone());

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

    // Verify metrics were recorded for the generate request.
    let snap = metrics.snapshot();
    let route = snap.routes.get("gpt-4o").expect("route should exist");
    assert_eq!(route.total_requests, 1);
    assert_eq!(route.total_errors, 0);
    assert_eq!(route.avg_input_tokens, Some(8));
    assert_eq!(route.avg_output_tokens, Some(4));
    let ep = route
        .by_endpoint
        .get("mock:gpt-4o")
        .expect("endpoint should exist");
    assert_eq!(ep.total_requests, 1);
}

#[tokio::test]
async fn responses_generate_messages_input() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let metrics = Arc::new(MetricsStore::new());
    let filter = responses_filter(table, router, metrics);

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
    let metrics = Arc::new(MetricsStore::new());
    let filter = responses_filter(table, router, metrics.clone());

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

    // Verify streaming request was counted (no token data for streams).
    let snap = metrics.snapshot();
    let route = snap.routes.get("gpt-4o").expect("route should exist");
    assert_eq!(route.total_requests, 1);
    assert_eq!(route.total_errors, 0);
    assert_eq!(route.avg_input_tokens, None);
}

#[tokio::test]
async fn responses_wrong_method() {
    let table = Arc::new(MockTable);
    let router = Arc::new(MockRouter);
    let metrics = Arc::new(MetricsStore::new());
    let filter = responses_filter(table, router, metrics);

    let res = warp::test::request()
        .method("GET")
        .path("/v1/responses")
        .reply(&filter)
        .await;

    assert_eq!(res.status(), 405);
}
