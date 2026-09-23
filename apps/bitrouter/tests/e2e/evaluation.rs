//! Phase 1 internal evaluation rail against a local mock provider.

use std::sync::{Arc, Mutex};
use std::time::Duration;

use async_trait::async_trait;
use bitrouter::assemble::build_app_with_registered_extensions;
use bitrouter::metering::{ModelPricing, PricingSource, calculate_charge_evidence};
use bitrouter_sdk::config::{self, Config};
use bitrouter_sdk::error::{BitrouterError, Result};
use bitrouter_sdk::evaluation::pipeline::{
    EvaluationAttemptRecord, EvaluationAttemptRecorder, EvaluationAttemptTerminal,
};
use bitrouter_sdk::evaluation::{EvaluationRequest, EvaluationResult, EvaluationRoutingTarget};
use bitrouter_sdk::extension::ExtensionApi;
use bitrouter_sdk::extension::evaluation_format::{
    EvaluationFormatAdapter, EvaluationFormatDescriptor,
};
use bitrouter_sdk::language_model::{Usage, UsageOrigin};
use sea_orm::{ColumnTrait, EntityTrait, QueryFilter};
use serde_json::{Value, json};
use wiremock::matchers::{header, method, path};
use wiremock::{Mock, MockServer, ResponseTemplate};

struct FixtureFormat {
    revision: u32,
}

impl EvaluationFormatAdapter for FixtureFormat {
    fn descriptor(&self) -> EvaluationFormatDescriptor {
        EvaluationFormatDescriptor {
            extension_id: "fixture".into(),
            adapter_id: "decisions".into(),
            revision: self.revision,
        }
    }

    fn render_request(
        &self,
        request: &EvaluationRequest,
        target: &EvaluationRoutingTarget,
    ) -> Result<Value> {
        Ok(json!({
            "model": target.provider_model_id,
            "state": request.state,
            "questions": request.questions,
        }))
    }

    fn parse_response(
        &self,
        body: Value,
        _request: &EvaluationRequest,
    ) -> Result<EvaluationResult> {
        serde_json::from_value(body).map_err(|_| BitrouterError::UpstreamInvalidResponse {
            message: "fixture response is invalid".into(),
        })
    }
}

#[derive(Default)]
struct CapturingRecorder {
    records: Mutex<Vec<EvaluationAttemptRecord>>,
}

impl CapturingRecorder {
    fn snapshot(&self) -> Result<Vec<EvaluationAttemptRecord>> {
        self.records
            .lock()
            .map(|records| records.clone())
            .map_err(|_| BitrouterError::internal("fixture recorder lock poisoned"))
    }
}

#[async_trait]
impl EvaluationAttemptRecorder for CapturingRecorder {
    async fn record(&self, record: EvaluationAttemptRecord) -> Result<Option<f64>> {
        let cost = record.usage.as_ref().and_then(|usage| {
            let tokens = Usage {
                prompt_tokens: usage.input_tokens,
                completion_tokens: usage.output_tokens,
                origin: UsageOrigin::ProviderReported,
                ..Usage::default()
            };
            let price = ModelPricing::new(1.0, 0.0);
            calculate_charge_evidence(&tokens, &price, PricingSource::Configured)
                .charge_micro_usd
                .map(|micro_usd| micro_usd as f64 / 1_000_000.0)
        });
        self.records
            .lock()
            .map_err(|_| BitrouterError::internal("fixture recorder lock poisoned"))?
            .push(record);
        Ok(cost)
    }
}

fn fixture_config(upstream: &MockServer, accounts: &str) -> anyhow::Result<Config> {
    Ok(config::parse(&format!(
        r#"
inherit_defaults: false
database:
  url: "sqlite::memory:"
providers:
  fixture:
    api_base: {}
    api_key: fixture-secret
    headers:
      x-static: "yes"
      x-forward:
        passthrough: true
    operations:
      evaluate:
        endpoint: /v1/decide
        format: {{ extension: fixture, adapter: decisions, revision: 1 }}
    models:
      - id: fixture/model
        provider_model_id: upstream-model
        operations:
          evaluate:
            question_types: [noul, choice, score]
            max_choice_options: 3
            max_score_levels: 3
        pricing:
          input_micro_usd_per_token: 1
          output_micro_usd_per_token: 0
    {accounts}
"#,
        upstream.uri()
    ))?)
}

fn question() -> anyhow::Result<EvaluationRequest> {
    Ok(serde_json::from_value(json!({
        "model": "fixture/model",
        "state": {"order_total": 42, "urgent": true},
        "questions": {
            "approve": {"type": "noul", "instructions": "approve this order?"}
        }
    }))?)
}

fn valid_upstream_answer() -> Value {
    json!({
        "id": "untrusted-upstream-id",
        "model": "upstream-model-v1",
        "provider": "untrusted-provider",
        "answers": {"approve": {"type": "noul", "noul": 0.75}},
        "usage": {"input_tokens": 5, "output_tokens": 2}
    })
}

pub(super) fn registered() -> Result<ExtensionApi> {
    let mut extensions = ExtensionApi::new();
    extensions.register_evaluation_format(Arc::new(FixtureFormat { revision: 1 }))?;
    Ok(extensions)
}

async fn wait_for_upstream_calls(upstream: &MockServer, count: usize) -> anyhow::Result<()> {
    tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            let calls = upstream
                .received_requests()
                .await
                .ok_or_else(|| anyhow::anyhow!("mock upstream capture unavailable"))?;
            if calls.len() >= count {
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await?
}

async fn assert_upstream_calls(upstream: &MockServer, expected: usize) -> anyhow::Result<()> {
    let calls = upstream
        .received_requests()
        .await
        .ok_or_else(|| anyhow::anyhow!("mock upstream capture unavailable"))?;
    anyhow::ensure!(
        calls.len() == expected,
        "expected {expected} upstream calls, received {}",
        calls.len()
    );
    Ok(())
}

#[tokio::test]
async fn internal_evaluation_keeps_format_auth_headers_and_metering_host_owned()
-> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .and(header("authorization", "Bearer fixture-secret"))
        .and(header("x-static", "yes"))
        .and(header("x-forward", "caller-value"))
        .respond_with(ResponseTemplate::new(200).set_body_json(valid_upstream_answer()))
        .mount(&upstream)
        .await;

    let config = fixture_config(&upstream, "")?;
    let recorder = Arc::new(CapturingRecorder::default());
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, Some(recorder.clone()))
            .await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let mut inbound = http::HeaderMap::new();
    inbound.insert("x-forward", http::HeaderValue::from_static("caller-value"));
    let result = pipeline
        .evaluate(question()?, "eval-1".into(), inbound)
        .await?;
    assert_eq!(result.id, "eval-1");
    assert_eq!(result.provider, "fixture");
    assert_eq!(result.model, "upstream-model-v1");
    assert_eq!(result.usage.cost, Some(0.000005));
    let records = recorder.snapshot()?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].terminal, EvaluationAttemptTerminal::Completed);
    assert_eq!(records[0].provider_model_id, "upstream-model");
    assert_upstream_calls(&upstream, 1).await?;
    Ok(())
}

#[tokio::test]
async fn invalid_native_binding_fails_before_database_assembly() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let mut config = fixture_config(&upstream, "")?;
    config.database.url = "invalid-scheme://never-open".into();
    let recorder = Arc::new(CapturingRecorder::default());
    let error = build_app_with_registered_extensions(
        &config,
        None,
        &ExtensionApi::new(),
        Some(recorder.clone()),
    )
    .await
    .err()
    .ok_or_else(|| anyhow::anyhow!("missing format unexpectedly assembled"))?;
    assert!(
        error
            .to_string()
            .contains("missing evaluation format extension")
    );
    let error = bitrouter::build_app(&config)
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("default bro host unexpectedly assembled"))?;
    assert!(
        error
            .to_string()
            .contains("missing evaluation format extension")
    );

    let mut wrong = ExtensionApi::new();
    wrong.register_evaluation_format(Arc::new(FixtureFormat { revision: 2 }))?;
    let error = build_app_with_registered_extensions(&config, None, &wrong, Some(recorder))
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("wrong revision unexpectedly assembled"))?;
    assert!(error.to_string().contains("revision mismatch"));
    Ok(())
}

#[tokio::test]
async fn default_host_recorder_persists_content_free_attempt_and_zero_output_price()
-> anyhow::Result<()> {
    use bitrouter::metering::entities::evaluation_attempts;

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .respond_with(ResponseTemplate::new(200).set_body_json(valid_upstream_answer()))
        .mount(&upstream)
        .await;
    let config = fixture_config(&upstream, "")?;
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, None).await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let result = pipeline
        .evaluate(question()?, "durable-eval-1".into(), http::HeaderMap::new())
        .await?;
    assert_eq!(result.usage.cost, Some(0.000005));
    let row = evaluation_attempts::Entity::find()
        .filter(evaluation_attempts::Column::RequestId.eq("durable-eval-1"))
        .one(&assembled.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("evaluation attempt was not persisted"))?;
    assert_eq!(row.terminal, "completed");
    assert_eq!(row.canonical_model.as_deref(), Some("fixture/model"));
    assert_eq!(row.caller_api_key_id.as_deref(), Some("anonymous"));
    assert_eq!(row.caller_user_id.as_deref(), Some("anonymous"));
    assert_eq!(row.charge_status, "computed");
    assert_eq!(row.charge_micro_usd, Some(5));
    assert_eq!(row.input_tokens.as_deref(), Some("5"));
    assert_eq!(row.output_tokens.as_deref(), Some("2"));
    assert_eq!(row.reported_model.as_deref(), Some("upstream-model-v1"));
    assert!(
        !row.charge_evidence_json
            .unwrap_or_default()
            .contains("approve")
    );
    assert_upstream_calls(&upstream, 1).await?;
    Ok(())
}

#[tokio::test]
async fn missing_price_remains_unknown_without_erasing_reported_output_usage() -> anyhow::Result<()>
{
    use bitrouter::metering::entities::evaluation_attempts;

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .respond_with(ResponseTemplate::new(200).set_body_json(valid_upstream_answer()))
        .mount(&upstream)
        .await;
    let mut config = fixture_config(&upstream, "")?;
    let provider = config
        .providers
        .get_mut("fixture")
        .ok_or_else(|| anyhow::anyhow!("fixture provider missing"))?;
    provider
        .models
        .first_mut()
        .ok_or_else(|| anyhow::anyhow!("fixture model missing"))?
        .pricing = None;
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, None).await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let result = pipeline
        .evaluate(question()?, "eval-no-price".into(), http::HeaderMap::new())
        .await?;
    assert_eq!(result.usage.cost, None);
    let row = evaluation_attempts::Entity::find()
        .filter(evaluation_attempts::Column::RequestId.eq("eval-no-price"))
        .one(&assembled.db)
        .await?
        .ok_or_else(|| anyhow::anyhow!("evaluation attempt was not persisted"))?;
    assert_eq!(row.charge_status, "unknown");
    assert_eq!(row.charge_micro_usd, None);
    assert_eq!(row.output_tokens.as_deref(), Some("2"));
    assert!(
        row.charge_evidence_json
            .unwrap_or_default()
            .contains("pricing_not_found")
    );
    Ok(())
}

#[tokio::test]
async fn provider_auth_rejection_is_one_terminal_attempt_without_account_retry()
-> anyhow::Result<()> {
    use bitrouter::metering::entities::evaluation_attempts;

    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .and(header("authorization", "Bearer first-secret"))
        .respond_with(ResponseTemplate::new(401).set_body_string("rejected"))
        .mount(&upstream)
        .await;
    let config = fixture_config(
        &upstream,
        "account_strategy: failover\n    accounts:\n      - { api_key: first-secret, label: first }\n      - { api_key: second-secret, label: second }",
    )?;
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, None).await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let result = pipeline
        .evaluate(question()?, "eval-401".into(), http::HeaderMap::new())
        .await;
    assert!(result.is_err());
    let rows = evaluation_attempts::Entity::find()
        .filter(evaluation_attempts::Column::RequestId.eq("eval-401"))
        .all(&assembled.db)
        .await?;
    assert_eq!(rows.len(), 1);
    let row = rows
        .first()
        .ok_or_else(|| anyhow::anyhow!("failed evaluation attempt was not persisted"))?;
    assert_eq!(row.terminal, "failed");
    assert_eq!(
        row.error_code.as_deref(),
        Some("upstream_authentication_failed")
    );
    assert_eq!(row.account_label.as_deref(), Some("first"));
    assert_upstream_calls(&upstream, 1).await?;
    Ok(())
}

#[tokio::test]
async fn malformed_first_account_retries_the_same_provider_and_records_both_attempts()
-> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .and(header("authorization", "Bearer first-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"invalid": true})))
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .and(header("authorization", "Bearer second-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(valid_upstream_answer()))
        .mount(&upstream)
        .await;
    let config = fixture_config(
        &upstream,
        "account_strategy: failover\n    accounts:\n      - { api_key: first-secret, label: first }\n      - { api_key: second-secret, label: second }",
    )?;
    let recorder = Arc::new(CapturingRecorder::default());
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, Some(recorder.clone()))
            .await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let result = pipeline
        .evaluate(question()?, "eval-retry".into(), http::HeaderMap::new())
        .await?;
    assert_eq!(result.usage.cost, Some(0.000005));
    let records = recorder.snapshot()?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].terminal, EvaluationAttemptTerminal::Failed);
    assert_eq!(records[0].error_code, Some("upstream_invalid_response"));
    assert_eq!(records[0].account_label.as_deref(), Some("first"));
    assert_eq!(records[1].terminal, EvaluationAttemptTerminal::Completed);
    assert_eq!(records[1].account_label.as_deref(), Some("second"));
    assert_upstream_calls(&upstream, 2).await?;
    Ok(())
}

#[tokio::test]
async fn provider_limits_reject_before_http_or_attempt_recording() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    let config = fixture_config(&upstream, "")?;
    let recorder = Arc::new(CapturingRecorder::default());
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, Some(recorder.clone()))
            .await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let request: EvaluationRequest = serde_json::from_value(json!({
        "model": "fixture/model",
        "state": "choose one",
        "questions": {
            "choice": {
                "type": "choice",
                "instructions": "pick",
                "criteria": {"a": null, "b": null, "c": null, "d": null}
            }
        }
    }))?;
    let error = pipeline
        .evaluate(request, "too-many".into(), http::HeaderMap::new())
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("provider limit was not enforced"))?;
    assert_eq!(error.status(), 400);
    assert!(recorder.snapshot()?.is_empty());
    assert!(
        upstream
            .received_requests()
            .await
            .ok_or_else(|| anyhow::anyhow!("mock upstream capture unavailable"))?
            .is_empty()
    );
    Ok(())
}

#[tokio::test]
async fn rate_limit_retries_only_the_next_account_and_keeps_retry_after_host_owned()
-> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .and(header("authorization", "Bearer first-secret"))
        .respond_with(
            ResponseTemplate::new(429)
                .insert_header("retry-after", "0")
                .set_body_string("limited"),
        )
        .mount(&upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .and(header("authorization", "Bearer second-secret"))
        .respond_with(ResponseTemplate::new(200).set_body_json(valid_upstream_answer()))
        .mount(&upstream)
        .await;
    let config = fixture_config(
        &upstream,
        "account_strategy: failover\n    accounts:\n      - { api_key: first-secret, label: first }\n      - { api_key: second-secret, label: second }",
    )?;
    let recorder = Arc::new(CapturingRecorder::default());
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, Some(recorder.clone()))
            .await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    pipeline
        .evaluate(question()?, "eval-429".into(), http::HeaderMap::new())
        .await?;
    let records = recorder.snapshot()?;
    assert_eq!(records.len(), 2);
    assert_eq!(records[0].error_code, Some("upstream_rate_limited"));
    assert_eq!(records[1].terminal, EvaluationAttemptTerminal::Completed);
    assert_upstream_calls(&upstream, 2).await?;
    Ok(())
}

#[tokio::test]
async fn total_deadline_is_host_owned_and_records_timeout() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(valid_upstream_answer()),
        )
        .mount(&upstream)
        .await;
    let mut config = fixture_config(&upstream, "")?;
    config.upstream.timeouts.total_secs = Some(1);
    let recorder = Arc::new(CapturingRecorder::default());
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, Some(recorder.clone()))
            .await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let error = pipeline
        .evaluate(question()?, "eval-timeout".into(), http::HeaderMap::new())
        .await
        .err()
        .ok_or_else(|| anyhow::anyhow!("slow provider unexpectedly completed"))?;
    assert!(matches!(error, BitrouterError::UpstreamTimeout));
    let records = recorder.snapshot()?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].terminal, EvaluationAttemptTerminal::TimedOut);
    assert_upstream_calls(&upstream, 1).await?;
    Ok(())
}

#[tokio::test]
async fn cancellation_records_unknown_remote_completion_and_shutdown_drains_it()
-> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_secs(2))
                .set_body_json(valid_upstream_answer()),
        )
        .mount(&upstream)
        .await;
    let config = fixture_config(&upstream, "")?;
    let recorder = Arc::new(CapturingRecorder::default());
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, Some(recorder.clone()))
            .await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let active = tokio::spawn({
        let pipeline = Arc::clone(&pipeline);
        async move {
            pipeline
                .evaluate(question()?, "eval-cancel".into(), http::HeaderMap::new())
                .await
                .map_err(anyhow::Error::from)
        }
    });
    wait_for_upstream_calls(&upstream, 1).await?;
    active.abort();
    let _ = active.await;
    tokio::time::timeout(Duration::from_secs(2), pipeline.drain()).await?;
    let records = recorder.snapshot()?;
    assert_eq!(records.len(), 1);
    assert_eq!(
        records[0].terminal,
        EvaluationAttemptTerminal::UnknownRemoteCompletion
    );
    assert_eq!(records[0].error_code, Some("client_cancelled"));
    let rejected = pipeline
        .evaluate(question()?, "after-drain".into(), http::HeaderMap::new())
        .await;
    assert!(matches!(rejected, Err(BitrouterError::UpstreamUnavailable)));
    assert_upstream_calls(&upstream, 1).await?;
    Ok(())
}

#[tokio::test]
async fn graceful_drain_waits_for_started_attempt_and_records_completion() -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/v1/decide"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_delay(Duration::from_millis(200))
                .set_body_json(valid_upstream_answer()),
        )
        .mount(&upstream)
        .await;
    let config = fixture_config(&upstream, "")?;
    let recorder = Arc::new(CapturingRecorder::default());
    let assembled =
        build_app_with_registered_extensions(&config, None, &registered()?, Some(recorder.clone()))
            .await?;
    let pipeline = assembled
        .evaluation_pipeline
        .as_ref()
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("evaluation pipeline was not assembled"))?;
    let active = tokio::spawn({
        let pipeline = Arc::clone(&pipeline);
        async move {
            pipeline
                .evaluate(question()?, "eval-drain".into(), http::HeaderMap::new())
                .await
                .map_err(anyhow::Error::from)
        }
    });
    wait_for_upstream_calls(&upstream, 1).await?;
    let draining = tokio::spawn({
        let pipeline = Arc::clone(&pipeline);
        async move { pipeline.drain().await }
    });
    tokio::time::sleep(Duration::from_millis(20)).await;
    assert!(
        !draining.is_finished(),
        "drain returned before the upstream attempt"
    );
    let result = active.await??;
    assert_eq!(result.id, "eval-drain");
    tokio::time::timeout(Duration::from_secs(2), draining).await??;
    let records = recorder.snapshot()?;
    assert_eq!(records.len(), 1);
    assert_eq!(records[0].terminal, EvaluationAttemptTerminal::Completed);
    assert_upstream_calls(&upstream, 1).await?;
    Ok(())
}
