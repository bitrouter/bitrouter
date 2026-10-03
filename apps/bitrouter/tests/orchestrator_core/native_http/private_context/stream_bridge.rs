//! Real SSE transport with the production private-context policy. Static fixture
//! authentication intentionally differs from the subscription OAuth adapter, whose
//! output-limit removal makes it ineligible for managed execution.
//! https://developers.openai.com/api/docs/guides/streaming-responses

use super::*;
#[path = "stream_bridge/failures.rs"]
mod failures;
use bitrouter::continuation::{ContinuationKeySource, native_context::PrivateContextPolicy};
use bitrouter_sdk::App;
use bitrouter_sdk::language_model::executor::HttpExecutor;
use bitrouter_sdk::language_model::native::InputTokenCounting;
use bitrouter_sdk::language_model::native_context::metadata;
use bitrouter_sdk::language_model::native_continuation::{
    CONTINUATION_FIELD, ContinuationFailure, NativeContinuationInput, NativeContinuationOutput,
    REQUIRED_STATE_FIELD,
};
use bitrouter_sdk::language_model::routing::StaticRoutingTable;
use bitrouter_sdk::language_model::types::{ApiProtocol, FinishReason, RoutingTarget};

fn bridge_prompt() -> Prompt {
    let mut value = prompt();
    value.model = "bridge".into();
    value
}

fn bridge_followup(content: Vec<Content>) -> Prompt {
    let mut value = followup(content);
    value.model = "bridge".into();
    value
}

fn app(
    home: &std::path::Path,
    upstream: &MockServer,
    key: &str,
    counting: bool,
) -> Result<Arc<App>> {
    configured_app(
        home,
        &upstream.uri(),
        key,
        counting,
        HttpExecutor::with_defaults()?,
    )
}

fn configured_app(
    home: &std::path::Path,
    upstream: &str,
    key: &str,
    counting: bool,
    executor: HttpExecutor,
) -> Result<Arc<App>> {
    let routes = StaticRoutingTable::new();
    routes.insert(
        "bridge",
        vec![RoutingTarget {
            provider_name: "openai-codex".into(),
            service_id: "served".into(),
            api_base: upstream.into(),
            api_key: key.into(),
            api_protocol: ApiProtocol::Responses,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            model_constraints: bitrouter_sdk::language_model::native::NativeRouteConstraints {
                input_token_counting: counting.then_some(InputTokenCounting::Responses),
                ..Default::default()
            },
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
            headers: Vec::new(),
        }],
    );
    let executor = Arc::new(executor);
    Ok(Arc::new(
        App::builder()
            .language_model(|builder| {
                builder
                    .routing_table(Arc::new(routes))
                    .executor(executor)
                    .native_private_context(Arc::new(PrivateContextPolicy::new(
                        ContinuationKeySource::lazy(home.to_path_buf()),
                    )));
            })
            .build()?,
    ))
}

fn reasoning(id: &str, text: &str) -> Value {
    json!({"type":"reasoning", "id":id, "status":"completed", "summary":[{"type":"summary_text", "text":text}], "encrypted_content":format!("encrypted-{id}")})
}

fn response(stored: bool, private: bool, lossy: bool) -> Value {
    let mut output = Vec::new();
    if private {
        output.push(reasoning("rs_first", "thought before"));
    }
    output.push(json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"answer"}]}));
    if private {
        output.push(reasoning("rs_second", "thought after"));
    }
    if lossy {
        output.push(json!({"type":"future_private_item", "opaque":"hidden-fixture-state"}));
    }
    json!({"id":"resp_stream_private", "status":"completed", "store":stored, "output":output,
        "usage":{"input_tokens":20,"output_tokens":6,"total_tokens":26,"input_tokens_details":{"cached_tokens":3},"output_tokens_details":{"reasoning_tokens":2}}})
}

fn events(response: &Value) -> String {
    // Deliberately incomplete delta projection. The full terminal must provide
    // ordered reasoning and final text without duplicating delta content.
    format!(
        "data: {}\n\ndata: {}\n\ndata: {}\n\n",
        json!({"type":"response.created", "response":{"id":response["id"]}}),
        json!({"type":"response.output_text.delta", "delta":"answer"}),
        json!({"type":if response["status"] == "incomplete" { "response.incomplete" } else { "response.completed" }, "response":response})
    )
}

async fn mount(upstream: &MockServer, body: String) {
    Mock::given(method("POST"))
        .and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(body, "text/event-stream"))
        .mount(upstream)
        .await;
    Mock::given(method("POST"))
        .and(path("/responses/input_tokens"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"object":"response.input_tokens","input_tokens":20})),
        )
        .mount(upstream)
        .await;
}

#[tokio::test]
async fn native_stream_bridge_preserves_private_order_and_stored_suffix_count_after_restart()
-> Result<()> {
    for stored in [false, true] {
        let home = tempfile::tempdir()?;
        let upstream = MockServer::start().await;
        mount(&upstream, events(&response(stored, true, false))).await;
        let capture = Arc::new(Capture::default());
        let first = app(home.path(), &upstream, "fixture-key", true)?
            .execute_native_controlled(bridge_prompt(), owner(), capture.clone())
            .await?;
        assert_eq!(first.result.content.len(), 3);
        assert!(
            matches!(&first.result.content[0], Content::Reasoning { text, .. } if text == "thought before")
        );
        assert!(matches!(&first.result.content[1], Content::Text { text, .. } if text == "answer"));
        assert!(
            matches!(&first.result.content[2], Content::Reasoning { text, .. } if text == "thought after")
        );
        assert_eq!(
            metadata(&first.result.content[0])["openai"]["reasoningItem"],
            reasoning("rs_first", "thought before")
        );
        assert_eq!(
            metadata(&first.result.content[2])["openai"]["reasoningItem"],
            reasoning("rs_second", "thought after")
        );
        assert!(first.result.response_id.is_none());
        assert_eq!(
            first
                .result
                .usage
                .as_ref()
                .context("usage")?
                .cache_read_tokens,
            3
        );
        {
            let reports = capture.reports.lock().await;
            assert_eq!(
                reports[0].private_context.output,
                PrivateContextEvidence::Verified { parts: 2 }
            );
            assert_eq!(
                reports[0].continuation.output,
                if stored {
                    NativeContinuationOutput::Issued
                } else {
                    NativeContinuationOutput::NotStored
                }
            );
            assert!(!serde_json::to_string(&reports[0])?.contains("resp_stream_private"));
        }
        let next = bridge_followup(first.result.content);
        app(home.path(), &upstream, "fixture-key", true)?
            .execute_native_controlled(next.clone(), owner(), capture.clone())
            .await?;
        let requests = upstream.received_requests().await.context("requests")?;
        assert_eq!(requests.len(), 4);
        let count: Value = serde_json::from_slice(&requests[2].body)?;
        let generation: Value = serde_json::from_slice(&requests[3].body)?;
        assert_eq!(generation["stream"], true);
        assert_eq!(count["input"], generation["input"]);
        assert_eq!(
            count["previous_response_id"],
            generation["previous_response_id"]
        );
        assert!(!generation.to_string().contains(CONTINUATION_FIELD));
        assert!(!generation.to_string().contains(ORIGIN_FIELD));
        if stored {
            assert_eq!(generation["previous_response_id"], "resp_stream_private");
            assert_eq!(
                generation["input"],
                json!([{"type":"message","role":"user","content":[{"type":"input_text","text":"continue"}]}])
            );
            assert_eq!(
                capture.reports.lock().await[1].continuation.input,
                NativeContinuationInput::Resumed { prefix_messages: 2 }
            );
        } else {
            assert!(generation.get("previous_response_id").is_none());
            assert_eq!(
                generation["input"][1],
                reasoning("rs_first", "thought before")
            );
            assert_eq!(
                generation["input"][3],
                reasoning("rs_second", "thought after")
            );
            assert_eq!(
                capture.reports.lock().await[1].private_context.input,
                PrivateContextEvidence::Verified { parts: 2 }
            );
        }
        let before = requests.len();
        let denied = app(home.path(), &upstream, "changed-key", true)?
            .execute_native_controlled(next, owner(), Arc::new(Capture::default()))
            .await;
        assert!(denied.is_err());
        assert_eq!(
            upstream
                .received_requests()
                .await
                .context("requests")?
                .len(),
            before
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_stream_bridge_retains_unknown_state_and_incomplete_usage() -> Result<()> {
    for stored in [false, true] {
        let home = tempfile::tempdir()?;
        let upstream = MockServer::start().await;
        let mut terminal = response(stored, false, true);
        terminal["status"] = "incomplete".into();
        terminal["incomplete_details"] = json!({"reason":"max_output_tokens"});
        mount(&upstream, events(&terminal)).await;
        let capture = Arc::new(Capture::default());
        let app = app(home.path(), &upstream, "fixture-key", false)?;
        let first = app
            .execute_native_controlled(bridge_prompt(), owner(), capture.clone())
            .await?;
        assert_eq!(first.result.finish_reason, Some(FinishReason::Length));
        assert_eq!(
            first
                .result
                .usage
                .as_ref()
                .context("usage")?
                .completion_tokens,
            6
        );
        assert!(first.result.content.iter().any(|part| {
            metadata(part)
                .get(ORIGIN_NAMESPACE)
                .is_some_and(|fields| fields.get(REQUIRED_STATE_FIELD).is_some())
        }));
        let next = app
            .execute_native_controlled(
                bridge_followup(first.result.content),
                owner(),
                capture.clone(),
            )
            .await;
        assert_eq!(next.is_ok(), stored);
        assert_eq!(
            upstream
                .received_requests()
                .await
                .context("requests")?
                .len(),
            if stored { 2 } else { 1 }
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_stream_bridge_rejects_invalid_terminal_before_sealing() -> Result<()> {
    let valid = response(true, true, false);
    let mut mismatch = valid.clone();
    mismatch["id"] = "different-response".into();
    let bodies = [
        "data: {\"type\":\"response.output_text.delta\",\"delta\":\"partial\"}\n\n".into(),
        format!(
            "{}data: {{\"type\":\"response.output_text.delta\",\"delta\":\"late\"}}\n\n",
            events(&valid)
        ),
        format!(
            "data: {}\n\ndata: {}\n\n",
            json!({"type":"response.created","response":{"id":"expected-response"}}),
            json!({"type":"response.completed","response":mismatch})
        ),
    ];
    for body in bodies {
        let home = tempfile::tempdir()?;
        let upstream = MockServer::start().await;
        mount(&upstream, body).await;
        let capture = Arc::new(Capture::default());
        let result = app(home.path(), &upstream, "fixture-key", false)?
            .execute_native_controlled(bridge_prompt(), owner(), capture.clone())
            .await;
        assert!(result.is_err());
        let reports = capture.reports.lock().await;
        assert_eq!(reports.len(), 1);
        assert!(reports[0].result.is_none());
        assert_eq!(
            reports[0].continuation.output,
            NativeContinuationOutput::Unknown
        );
        assert_eq!(
            reports[0].private_context.output,
            PrivateContextEvidence::Unknown
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_stream_bridge_outbound_no_store_and_missing_terminal_output_remain_explicit()
-> Result<()> {
    for missing_output in [false, true] {
        let home = tempfile::tempdir()?;
        let upstream = MockServer::start().await;
        let mut terminal = response(true, false, false);
        if missing_output {
            terminal
                .as_object_mut()
                .context("response")?
                .remove("output");
        }
        mount(&upstream, events(&terminal)).await;
        let mut input = bridge_prompt();
        input.params.store = Some(false);
        let capture = Arc::new(Capture::default());
        let app = app(home.path(), &upstream, "fixture-key", false)?;
        let result = app
            .execute_native_controlled(input.clone(), owner(), capture.clone())
            .await?;
        assert!(
            matches!(&result.result.content[0], Content::Text { text, .. } if text == "answer")
        );
        assert_eq!(
            result.result.usage.as_ref().context("usage")?.prompt_tokens,
            20
        );
        assert_eq!(
            capture.reports.lock().await[0].continuation.output,
            if missing_output {
                NativeContinuationOutput::Unverified {
                    reason: ContinuationFailure::AttemptUnverified,
                }
            } else {
                NativeContinuationOutput::NotStored
            }
        );
        assert!(
            metadata(&result.result.content[0])
                .get(ORIGIN_NAMESPACE)
                .is_none_or(|fields| fields.get(CONTINUATION_FIELD).is_none())
        );
        let has_required = result.result.content.iter().any(|part| {
            metadata(part)
                .get(ORIGIN_NAMESPACE)
                .is_some_and(|fields| fields.get(REQUIRED_STATE_FIELD).is_some())
        });
        assert_eq!(has_required, missing_output);
        input.messages.push(Message {
            role: Role::Assistant,
            content: result.result.content,
        });
        input.messages.push(Message::text(Role::User, "continue"));
        let next = app.execute_native_controlled(input, owner(), capture).await;
        assert_eq!(next.is_err(), missing_output);
        assert_eq!(
            upstream
                .received_requests()
                .await
                .context("requests")?
                .len(),
            if missing_output { 1 } else { 2 }
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_stream_bridge_core_retains_complete_history_and_private_receipts() -> Result<()> {
    let home = tempfile::tempdir()?;
    let upstream = MockServer::start().await;
    mount(&upstream, events(&response(true, true, false))).await;
    let grant = OwnershipGrant {
        session_id: "bridge-session".into(),
        harness_id: "bridge-harness".into(),
        core_instance_id: "bridge-core".into(),
        execution_epoch: 1,
    };
    let harness = Arc::new(crate::Harness {
        grant: grant.clone(),
        store: Mutex::new(crate::Store::default()),
    });
    let limits = Limits::default();
    let session = CoreSession::bind(
        Bind {
            grant,
            durable_head: DurableHead::default(),
            checkpoint: None,
            limits: limits.clone(),
            manifest: HarnessManifest {
                tool_manifest_digest: HarnessManifest::digest(&[])?,
                tools: Vec::new(),
                workspace_id: "workspace".into(),
                workspace_revision: None,
                permission_revision: 1,
                max_tool_output_bytes: 8192,
                artifact_quota_bytes: 1024 * 1024,
                max_artifact_chunk_bytes: 8192,
                required_features: Vec::new(),
            },
        },
        &Capabilities {
            version: 1,
            core_instance_id: "bridge-core".into(),
            operations: Vec::new(),
            transports: vec!["in_process".into()],
            unsupported_features: Vec::new(),
            limits,
            max_sessions: 16,
            max_host_model_attempts: 16,
        },
        app(home.path(), &upstream, "fixture-key", false)?,
        owner(),
        harness,
    )
    .await?;
    let mut history = Vec::new();
    for (index, text) in ["first", "second"].into_iter().enumerate() {
        let mut input = crate::input(text);
        input.model = "bridge".into();
        session
            .start(
                &format!("task-{index}"),
                session.head().await.state_revision,
                input,
            )
            .await?;
        let done = session.drive().await?;
        assert_eq!(
            done.run.as_ref().context("run")?.status,
            RunStatus::Completed
        );
        let next_history = &done.agents[&done.agent_id].history;
        assert!(next_history.starts_with(&history));
        let attempt = &done.root_turn().context("turn")?.steps[0].attempts[0];
        let report = &attempt.receipt.as_ref().context("receipt")?.report;
        assert_eq!(report.continuation.output, NativeContinuationOutput::Issued);
        assert_eq!(
            report.private_context.output,
            PrivateContextEvidence::Verified { parts: 2 }
        );
        assert_eq!(
            report.result.as_ref().context("result")?.content,
            next_history.last().context("assistant")?.content
        );
        assert!(
            attempt
                .provider_work
                .iter()
                .all(|work| work.report.is_some())
        );
        if index > 0 {
            assert_eq!(
                report.continuation.input,
                NativeContinuationInput::Resumed {
                    prefix_messages: history.len() as u64
                }
            );
        }
        assert!(!serde_json::to_string(&done)?.contains("resp_stream_private"));
        history = next_history.clone();
    }
    let requests = upstream.received_requests().await.context("requests")?;
    assert_eq!(requests.len(), 2);
    let resumed: Value = serde_json::from_slice(&requests[1].body)?;
    assert_eq!(resumed["previous_response_id"], "resp_stream_private");
    assert_eq!(resumed["input"].as_array().context("input")?.len(), 1);
    assert!(!resumed.to_string().contains("encrypted-rs_first"));
    Ok(())
}

#[tokio::test]
async fn native_stream_bridge_cannot_certify_delta_only_private_state() -> Result<()> {
    for malformed in [None, Some(Value::Null), Some(json!("not-an-array"))] {
        let home = tempfile::tempdir()?;
        let upstream = MockServer::start().await;
        let mut terminal = response(true, true, false);
        match malformed {
            Some(value) => terminal["output"] = value,
            None => {
                terminal
                    .as_object_mut()
                    .context("response")?
                    .remove("output");
            }
        }
        let body = format!(
            "data: {}\n\n{}",
            json!({"type":"response.reasoning_summary_text.delta","delta":"private summary"}),
            events(&terminal)
        );
        mount(&upstream, body).await;
        let capture = Arc::new(Capture::default());
        let app = app(home.path(), &upstream, "fixture-key", false)?;
        let first = app
            .execute_native_controlled(bridge_prompt(), owner(), capture.clone())
            .await?;
        assert_eq!(
            first
                .result
                .usage
                .as_ref()
                .context("usage")?
                .completion_tokens,
            6
        );
        assert!(
            matches!(&first.result.content[0], Content::Reasoning { text, .. } if text == "private summary")
        );
        assert_eq!(
            capture.reports.lock().await[0].continuation.output,
            NativeContinuationOutput::Unverified {
                reason: ContinuationFailure::AttemptUnverified
            }
        );
        assert!(matches!(
            capture.reports.lock().await[0].private_context.output,
            PrivateContextEvidence::Unverified { .. }
        ));
        for part in &first.result.content {
            assert!(
                !metadata(part)
                    .get(ORIGIN_NAMESPACE)
                    .is_some_and(|fields| fields.get(CONTINUATION_FIELD).is_some()
                        || fields.get(ORIGIN_FIELD).is_some())
            );
        }
        let next = app
            .execute_native_controlled(bridge_followup(first.result.content), owner(), capture)
            .await;
        assert!(next.is_err());
        assert_eq!(
            upstream
                .received_requests()
                .await
                .context("requests")?
                .len(),
            1
        );
    }
    Ok(())
}
