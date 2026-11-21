use std::time::Duration;

use serde_json::json;
use wiremock::{
    Mock, MockServer, ResponseTemplate,
    matchers::{body_partial_json, method, path},
};

use crate::{
    auth::{
        db::{self, NewApiKey},
        keys,
    },
    gateway::http::{AppState, build_router},
    trajectory::{
        store::TrajectoryStore,
        types::{RequestStatus, TrajectoryEventKind},
    },
};

struct ServerTask(tokio::task::JoinHandle<std::io::Result<()>>);

impl Drop for ServerTask {
    fn drop(&mut self) {
        self.0.abort();
    }
}

#[tokio::test]
async fn http_preserves_usage_absence_and_zero_and_captures_fresh_trajectories()
-> anyhow::Result<()> {
    for enabled in [false, true] {
        exercise_http(enabled).await?;
    }
    Ok(())
}

async fn exercise_http(enabled: bool) -> anyhow::Result<()> {
    let upstream = MockServer::start().await;
    for known in [false, true] {
        let mut response = json!({
            "id":"provider-response", "object":"chat.completion", "model":"economy",
            "choices":[{"index":0,"message":{"role":"assistant","content":"done"},"finish_reason":"stop"}]
        });
        if known {
            response["usage"] = json!({"prompt_tokens":0,"completion_tokens":0,"total_tokens":0});
        }
        Mock::given(method("POST"))
            .and(path("/chat/completions"))
            .and(body_partial_json(
                json!({"messages":[{"role":"user","content":private_text(known)}]}),
            ))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(response)
                    .set_delay(Duration::from_millis(30)),
            )
            .expect(2)
            .mount(&upstream)
            .await;
    }
    let home = tempfile::tempdir()?;
    let config_path = home.path().join("bitrouter.yaml");
    let config = crate::config::parse_with(
        &format!(
            r#"inherit_defaults: false
database:
  url: "sqlite::memory:"
trajectory:
  enabled: {enabled}
policy:
  path: ./policy.yaml
providers:
  vendor:
    api_base: "{}"
    api_key: upstream-fixture-secret
    api_protocol:
      - "*": chat_completions
    models:
      - id: economy
routers:
  coding:
    selection:
      kind: policy
      base_model: vendor:economy
      policy: coding
"#,
            upstream.uri()
        ),
        |_| None,
    )?;
    tokio::fs::write(home.path().join("policy.yaml"), "spec_version: policy/1.0.0-beta.1\npolicies:\n  coding:\n    tiers:\n      economy: {model: vendor:economy}\n    default_tier: economy\n").await?;
    let assembled = crate::assemble::build_app_with_path(&config, Some(&config_path)).await?;
    let router = build_router(AppState {
        language_model: assembled
            .app
            .language_model()
            .ok_or_else(|| anyhow::anyhow!("missing pipeline"))?
            .clone(),
        mcp: None,
        skip_auth: false,
        metrics_renderer: None,
        prompt_transforms: Vec::new(),
    });
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let url = format!("http://{}/v1/chat/completions", listener.local_addr()?);
    let _server = ServerTask(tokio::spawn(
        async move { axum::serve(listener, router).await },
    ));
    let client = reqwest::Client::builder()
        .no_proxy()
        .timeout(Duration::from_secs(10))
        .build()?;
    let store = TrajectoryStore::new(assembled.db.clone());
    let refused = client.post(&url)
        .json(&json!({"model":"bitrouter/coding","messages":[{"role":"user","content":"unauthenticated"}]}))
        .send().await?;
    assert!(refused.status().is_client_error());
    {
        use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
        let count = assembled
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                "SELECT COUNT(*) AS count FROM trajectory_requests".to_owned(),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing trajectory count"))?;
        assert_eq!(count.try_get::<i64>("", "count")?, 0);
    }
    let correlation_key = enabled
        .then(|| crate::paths::get_or_create_correlation_key(home.path()))
        .transpose()?;
    for owner in ["owner-a", "owner-b"] {
        db::upsert_user(&assembled.db, owner).await?;
        let key = keys::generate();
        db::insert_api_key(
            &assembled.db,
            &NewApiKey {
                id: format!("key-{owner}"),
                key_hash: key.hash,
                user_id: owner.into(),
                spend_limit_micro_usd: None,
                rpm_limit: None,
                policy_id: None,
            },
        )
        .await?;
        for known in [false, true] {
            let response = client.post(&url).bearer_auth(&key.secret)
                .json(&json!({"model":"bitrouter/coding","messages":[{"role":"user","content":private_text(known)}]}))
                .send().await?;
            let status = response.status();
            let request_id = response
                .headers()
                .get("x-bitrouter-request-id")
                .ok_or_else(|| anyhow::anyhow!("missing gateway identity"))?
                .to_str()?
                .to_owned();
            let body = response.text().await?;
            assert!(status.is_success(), "{status}: {body}");
            let expected_origin = if known {
                "provider_reported"
            } else {
                "unknown"
            };
            if let Some(identity_key) = &correlation_key {
                let identity = identity_key.request_identity(owner, &request_id)?;
                let request = store
                    .request(owner, &identity)
                    .await?
                    .ok_or_else(|| anyhow::anyhow!("fresh HTTP request was not captured"))?;
                assert_eq!(request.status, RequestStatus::Settled);
                assert!(store.request("other-owner", &identity).await?.is_none());
                let events = store.events_for_episode(owner, &request.episode_id).await?;
                assert_eq!(events.len(), 2);
                assert_eq!(events[0].kind, TrajectoryEventKind::RequestStarted);
                assert_eq!(events[1].kind, TrajectoryEventKind::RequestSettled);
                let evidence = &events[1].evidence;
                assert_eq!(
                    evidence
                        .categorical
                        .get("settlement.usage_origin")
                        .map(String::as_str),
                    Some(expected_origin)
                );
                assert_eq!(
                    evidence.structural.get("settlement.total_tokens").copied(),
                    known.then_some(0)
                );
                assert_eq!(
                    evidence.structural.get("settlement.prompt_tokens").copied(),
                    known.then_some(0)
                );
                assert!(evidence.structural["settlement.duration_ms"] >= 30);
                let persisted = serde_json::to_string(&events)?;
                assert!(!persisted.contains(private_text(known)));
                assert!(!persisted.contains("upstream-fixture-secret"));
                assert!(!persisted.contains(&key.secret));
                assert!(
                    store
                        .events_for_episode("other-owner", &request.episode_id)
                        .await?
                        .is_empty()
                );
            }
            #[cfg(feature = "eval-publication")]
            {
                if let Some(publisher) = &assembled.trajectory_outbox_publisher {
                    publisher.wait_for_idle().await;
                }
                let subjects = assembled
                    .eval_service
                    .store()
                    .list_subjects_for_owner(owner)
                    .await?;
                let subject = if let Some(key) = &correlation_key {
                    let identity = key.request_identity(owner, &request_id)?;
                    let request = store
                        .request(owner, &identity)
                        .await?
                        .ok_or_else(|| anyhow::anyhow!("missing request"))?;
                    subjects
                        .iter()
                        .find(|subject| subject.subject_id == request.episode_id)
                } else {
                    subjects
                        .iter()
                        .find(|subject| subject.subject_id == request_id)
                }
                .ok_or_else(|| {
                    anyhow::anyhow!("actual HTTP settlement did not close evaluation")
                })?;
                if enabled {
                    assert_eq!(subject.scope, bitrouter_ai::eval::EvalScope::Episode);
                    assert!(subject.decisions.is_empty());
                    let results = assembled
                        .eval_service
                        .store()
                        .results_for_subject(&subject.eval_id)
                        .await?;
                    assert_eq!(results.len(), 1);
                    let metrics = &results[0].result.metrics;
                    assert!(metrics["latency.ms"].value >= 30);
                    assert_eq!(
                        metrics
                            .get("trajectory.total_tokens")
                            .map(|value| value.value),
                        known.then_some(0)
                    );
                } else {
                    assert_eq!(subject.scope, bitrouter_ai::eval::EvalScope::Request);
                    assert_eq!(subject.decisions.len(), 1);
                    assert_eq!(subject.decisions[0].selected_tier, "economy");
                    let evidence = subject
                        .evidence
                        .iter()
                        .find(|item| item.kind == "request.outcome")
                        .ok_or_else(|| anyhow::anyhow!("missing outcome evidence"))?;
                    assert_eq!(
                        evidence.attributes.get("usage_origin").map(String::as_str),
                        Some(expected_origin)
                    );
                    for name in ["prompt_tokens", "completion_tokens", "reasoning_tokens"] {
                        assert_eq!(
                            evidence.attributes.get(name).map(String::as_str),
                            known.then_some("0")
                        );
                    }
                }
                assert!(
                    assembled
                        .eval_service
                        .store()
                        .list_subjects_for_owner("other-owner")
                        .await?
                        .is_empty()
                );
            }
        }
        #[cfg(feature = "eval-publication")]
        assert_eq!(
            assembled
                .eval_service
                .store()
                .list_subjects_for_owner(owner)
                .await?
                .len(),
            2
        );
    }
    assert!(assembled.pending_eval_decisions.is_empty());
    assert_eq!(assembled.response_observer.buffered_request_count(), 0);
    Ok(())
}

fn private_text(known: bool) -> &'static str {
    if known {
        "private-zero-request"
    } else {
        "private-unknown-request"
    }
}

#[test]
fn capture_budget_rejects_encoded_input_before_allocating_a_copy() -> anyhow::Result<()> {
    assert!(serde_json::to_writer(&mut super::CaptureBudget(4), "ab").is_ok());
    assert!(serde_json::to_writer(&mut super::CaptureBudget(3), "ab").is_err());
    Ok(())
}

#[tokio::test]
async fn capture_is_idempotent_and_rejects_changed_lifecycle_ownership() -> anyhow::Result<()> {
    use crate::gateway::{
        caller::CallerContext,
        pipeline::{context::PipelineContext, hooks::PreRequestHook, types::PipelineRequest},
    };
    use crate::trajectory::{
        canonical::{Canonicalizer, CorrelationKey},
        correlation::TrajectoryRuntime,
        settlement::TrajectorySettlementRecorder,
    };
    use bitrouter_ai::types::{ApiProtocol, GenerationParams, Message, Prompt, Role};

    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let store = TrajectoryStore::new(db);
    let key = CorrelationKey::from_bytes(rand::random())?;
    let capture = super::CaptureIngress::new(TrajectoryRuntime::new(
        store.clone(),
        Canonicalizer::new(key.clone()),
    ));
    let prompt = Prompt {
        model: "vendor:model".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "private capture text")],
        tools: Vec::new(),
        params: GenerationParams::default(),
        response_format: None,
        tool_choice: None,
        stream: false,
    };
    let mut request = PipelineRequest::new(
        "vendor:model",
        CallerContext::new("key-a", "owner-a"),
        prompt,
    );
    request.inbound_protocol = Some(ApiProtocol::ChatCompletions);
    let mut context = PipelineContext::new(request);
    capture.check(&mut context).await?;
    capture.check(&mut context).await?;
    let binding = context
        .extension::<super::CapturedRequest>()
        .ok_or_else(|| anyhow::anyhow!("capture binding absent"))?;
    assert_eq!(context.get_events::<super::CapturedRequest>().len(), 1);
    assert_eq!(
        store
            .events_for_episode("owner-a", &binding.episode_id)
            .await?
            .len(),
        1
    );
    let mut settlement = context.settlement_context();
    settlement.caller = CallerContext::new("key-b", "owner-a");
    let recorder = TrajectorySettlementRecorder::new(store.clone(), None, key);
    assert!(recorder.record_if_tracked(&settlement, None).await.is_err());
    context.set_caller(CallerContext::new("key-a", "other-owner"));
    assert!(capture.revalidate_context(&context).await.is_err());
    assert!(capture.check(&mut context).await.is_err());
    assert!(
        store
            .request("other-owner", &binding.request_id)
            .await?
            .is_none()
    );
    assert_eq!(
        store
            .request("owner-a", &binding.request_id)
            .await?
            .ok_or_else(|| anyhow::anyhow!("request disappeared"))?
            .status,
        RequestStatus::Started
    );
    Ok(())
}
