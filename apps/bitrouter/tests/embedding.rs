//! Supported configuration and lifecycle APIs used by an independent host.

use anyhow::{Context, Result};
use bitrouter::config::{
    PricingConfig, PricingEndpointProfile, ProviderModel,
    decision::{DecisionModelConfig, DecisionPolicy, DecisionPricing},
    model::{InputTokenCounting, ModelTokenLimits, ReasoningEffortConfig},
};
use bitrouter_ai::types::ReasoningEffort;
use serde_json::json;

#[test]
fn configuration_fields_can_be_constructed_without_runtime_modules() -> Result<()> {
    let mut model: ProviderModel = serde_json::from_value(json!({"id": "fixture"}))?;
    model.token_limits = ModelTokenLimits {
        max_input_tokens: Some(4096),
        max_output_tokens: Some(1024),
        context_window: Some(8192),
    };
    model.input_token_counting = Some(InputTokenCounting::Responses);
    model.reasoning_effort = Some(ReasoningEffortConfig {
        levels: vec![ReasoningEffort::Low, ReasoningEffort::High],
        default: Some(ReasoningEffort::Low),
    });
    model.pricing = Some(PricingConfig {
        endpoint_profile: Some(PricingEndpointProfile::OpenaiGlobal),
        ..Default::default()
    });
    let decision = DecisionModelConfig {
        model: "classifier".into(),
        base_url: "https://classifier.invalid".into(),
        api_key_env: "CLASSIFIER_KEY".into(),
        timeout_ms: 1000,
        max_response_bytes: 4096,
        policy: DecisionPolicy::default(),
        pricing: Some(DecisionPricing {
            input_usd_per_million: 1.0,
            output_usd_per_million: 2.0,
        }),
    };
    decision.validate()?;
    let effort = model
        .reasoning_effort
        .as_ref()
        .context("reasoning declaration")?;
    effort.validate().map_err(anyhow::Error::msg)?;
    assert_eq!(
        serde_json::to_value(model.input_token_counting)?,
        json!("responses")
    );
    assert_eq!(
        serde_json::to_value(effort)?,
        json!({"levels": ["low", "high"], "default": "low"})
    );
    assert!(
        serde_json::from_value::<ReasoningEffortConfig>(json!({
            "levels": ["low"], "unsupported": true
        }))
        .is_err()
    );
    Ok(())
}

#[cfg(feature = "otel-http")]
mod shutdown {
    use std::{sync::Arc, time::Duration};

    use super::*;
    use axum::{body::Bytes, extract::State, http::StatusCode, routing::post};
    use bitrouter::assemble::build_app_with_extensions;
    use opentelemetry_proto::tonic::collector::trace::v1::ExportTraceServiceRequest;
    use prost::Message;
    use tokio::sync::{Mutex, Notify};
    use tokio_util::sync::CancellationToken;
    use wiremock::{
        Mock, MockServer, ResponseTemplate,
        matchers::{method, path},
    };

    #[derive(Clone, Default)]
    struct Collector {
        bodies: Arc<Mutex<Vec<Bytes>>>,
        received: Arc<Notify>,
        release: CancellationToken,
    }

    async fn collect(State(state): State<Collector>, body: Bytes) -> StatusCode {
        state.bodies.lock().await.push(body);
        state.received.notify_one();
        state.release.cancelled().await;
        StatusCode::OK
    }

    #[tokio::test]
    async fn custom_listener_shutdown_waits_for_buffered_trace_export() -> Result<()> {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "id": "reply", "object": "chat.completion", "model": "fixture", "created": 1,
                "choices": [{"index": 0, "message": {"role": "assistant", "content": "done"}, "finish_reason": "stop"}],
                "usage": {"prompt_tokens": 3, "completion_tokens": 1, "total_tokens": 4}
            })))
            .expect(1)
            .mount(&upstream)
            .await;
        let collector = Collector::default();
        let collector_listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let endpoint = format!("http://{}", collector_listener.local_addr()?);
        let collector_stop = CancellationToken::new();
        let collector_done = collector_stop.clone();
        let router = axum::Router::new()
            .route("/v1/traces", post(collect))
            .with_state(collector.clone());
        let collector_task = tokio::spawn(async move {
            axum::serve(collector_listener, router)
                .with_graceful_shutdown(collector_done.cancelled_owned())
                .await
        });
        let result: Result<()> = async {
            let config = bitrouter::config::parse(&serde_json::to_string(&json!({
                "inherit_defaults": false,
                "server": {"skip_auth": true},
                "database": {"url": "sqlite::memory:"},
                "providers": {"fixture": {
                    "api_base": upstream.uri(), "api_key": "fixture-key",
                    "api_protocol": "chat_completions", "models": [{"id": "fixture"}]
                }},
                "plugins": {"bitrouter-telemetry": {"otel": {
                    "endpoint": endpoint,
                    "traces": {"batch": {"flush_ms": 300_000}},
                    "metrics": {"enabled": false}
                }}}
            }))?)?;
            let assembled = build_app_with_extensions(&config, None, |_| Ok(())).await?;
            anyhow::ensure!(
                assembled.otel_init_error.is_none(),
                "exporter failed to initialize"
            );
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
            let url = format!("http://{}/v1/chat/completions", listener.local_addr()?);
            let stop = CancellationToken::new();
            let serving = assembled
                .app
                .serve_listener_with_router_wrapper_and_shutdown(
                    listener,
                    |router| router,
                    stop.clone().cancelled_owned(),
                );
            let request = async {
                let response = reqwest::Client::new().post(url).json(&json!({
                    "model": "fixture:fixture", "messages": [{"role": "user", "content": "hello"}]
                })).send().await;
                stop.cancel();
                response?
                    .error_for_status()?
                    .json::<serde_json::Value>()
                    .await
            };
            let (server_result, response) = tokio::time::timeout(Duration::from_secs(10), async {
                tokio::join!(serving, request)
            })
            .await
            .context("custom listener did not drain")?;
            server_result?;
            assert_eq!(response?["choices"][0]["message"]["content"], "done");

            let shutdown = assembled.shutdown();
            tokio::pin!(shutdown);
            tokio::time::timeout(Duration::from_secs(10), async {
                tokio::select! {
                    result = &mut shutdown => {
                        result?;
                        anyhow::bail!("shutdown completed before the collector acknowledged export")
                    }
                    () = collector.received.notified() => Ok(()),
                }
            })
            .await
            .context("shutdown did not flush the buffered spans")??;
            collector.release.cancel();
            tokio::time::timeout(Duration::from_secs(10), shutdown)
                .await
                .context("shutdown did not finish after export acknowledgement")??;
            let bodies = collector.bodies.lock().await;
            let mut spans = Vec::new();
            for body in bodies.iter() {
                let exported = ExportTraceServiceRequest::decode(body.clone())?;
                spans.extend(
                    exported
                        .resource_spans
                        .into_iter()
                        .flat_map(|resource| resource.scope_spans)
                        .flat_map(|scope| scope.spans),
                );
            }
            assert!(spans.iter().any(|span| span.name == "chat fixture"));
            assert!(spans.iter().any(|span| span.name == "settle"));
            Ok(())
        }
        .await;
        collector.release.cancel();
        collector_stop.cancel();
        let cleanup = tokio::time::timeout(Duration::from_secs(10), collector_task).await;
        result?;
        cleanup.context("collector did not stop")???;
        Ok(())
    }
}
