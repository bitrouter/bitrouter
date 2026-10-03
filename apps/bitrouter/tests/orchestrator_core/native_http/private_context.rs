//! Production private-history proof and wire round trips. Provider fixtures are
//! authored independently of the gateway renderers.
//! https://platform.claude.com/docs/en/build-with-claude/extended-thinking
//! https://ai.google.dev/gemini-api/docs/thought-signatures
//! https://developers.openai.com/api/docs/guides/reasoning

use super::*;
#[path = "private_context/continuation.rs"]
mod continuation;
#[path = "private_context/stream_bridge.rs"]
mod stream_bridge;
use bitrouter_sdk::language_model::native_context::{
    ORIGIN_FIELD, ORIGIN_NAMESPACE, PrivateContextEvidence, is_private, metadata_mut,
};
use bitrouter_sdk::language_model::types::{Content, ToolResultOutput};

struct PrivateFixture {
    app: bitrouter::assemble::Assembled,
    home: tempfile::TempDir,
    source: String,
    upstream: MockServer,
    checked: Arc<std::sync::Mutex<Vec<Input>>>,
}

impl PrivateFixture {
    async fn new(protocol: &str) -> Result<Self> {
        let upstream = MockServer::start().await;
        let (path_, output) = if protocol == "messages" {
            (
                "/messages",
                json!({
                    "id":"msg_private", "type":"message", "role":"assistant", "model":"served",
                    "content":[
                        {"type":"thinking", "thinking":"readable thought", "signature":"signed-fixture"},
                        {"type":"redacted_thinking", "data":"opaque-secret-fixture"},
                        {"type":"text", "text":"done"}],
                    "stop_reason":"end_turn", "usage":{"input_tokens":12,"output_tokens":5}
                }),
            )
        } else if protocol == "responses" {
            (
                "/responses",
                json!({
                    "id":"resp_private", "object":"response", "status":"completed", "model":"served",
                    "output":[
                        {"type":"reasoning", "id":"rs_first", "summary":[{"type":"summary_text", "text":"readable thought"}], "encrypted_content":"signed-fixture", "status":"completed"},
                        {"type":"function_call", "id":"fc_inspect", "call_id":"call_inspect", "name":"inspect", "arguments":"{\"path\":\"fixture\"}"},
                        {"type":"reasoning", "id":"rs_empty", "summary":[], "encrypted_content":"opaque-secret-fixture"},
                        {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]}
                    ],
                    "usage":{"input_tokens":12,"output_tokens":5,"total_tokens":17}
                }),
            )
        } else {
            (
                "/models/served:generateContent",
                json!({
                    "candidates":[{"content":{"role":"model", "parts":[
                        {"text":"readable thought", "thought":true, "thoughtSignature":"signed-fixture"},
                        {"functionCall":{"name":"inspect", "args":{"path":"fixture"}}, "thoughtSignature":"tool-signature-fixture"},
                    {"text":"final text", "thoughtSignature":"text-signature-fixture"},
                    {"inlineData":{"mimeType":"image/png", "data":"AQID"}, "thought":true, "thoughtSignature":"inline-signature-fixture"},
                    {"fileData":{"mimeType":"image/png", "fileUri":"https://example.invalid/fixture.png"}, "thought":true, "thoughtSignature":"file-signature-fixture"}
                    ]}, "finishReason":"STOP"}],
                    "usageMetadata":{"promptTokenCount":12,"candidatesTokenCount":5,"totalTokenCount":17}
                }),
            )
        };
        Mock::given(method("POST"))
            .and(path(path_))
            .respond_with(ResponseTemplate::new(200).set_body_json(output))
            .mount(&upstream)
            .await;
        let source = format!(
            r#"
inherit_defaults: false
registry:
  enabled: false
server:
  skip_auth: true
database:
  url: 'sqlite::memory:'
providers:
  fixture:
    api_base: {}
    api_key: fixture-key
    models:
      - id: served
        api_protocol: {protocol}
        capabilities: [tools]
      - id: changed
        api_protocol: {protocol}
        capabilities: [tools]
models:
  private:
    endpoints:
      - {{provider: fixture, service_id: served}}
checkers:
  private:
    native:
      revision: v1
routers:
  private:
    selection:
      kind: model
      model: private
    checks:
      request:
        - checker: private
          timeout_ms: 5000
"#,
            upstream.uri()
        );
        let home = tempfile::tempdir()?;
        let checked = Arc::new(std::sync::Mutex::new(Vec::new()));
        let app = assemble(&source, &home, checked.clone()).await?;
        Ok(Self {
            app,
            home,
            source,
            upstream,
            checked,
        })
    }

    fn checks(&self) -> Result<Vec<Input>> {
        self.checked
            .lock()
            .map(|rows| rows.clone())
            .map_err(|_| anyhow::anyhow!("fixture lock poisoned"))
    }
}

async fn assemble(
    source: &str,
    home: &tempfile::TempDir,
    checked: Arc<std::sync::Mutex<Vec<Input>>>,
) -> Result<bitrouter::assemble::Assembled> {
    let config_path = home.path().join("bitrouter.yaml");
    std::fs::write(&config_path, source)?;
    let config = bitrouter_sdk::config::parse_with(source, |_| None)?;
    bitrouter::assemble::build_app_with_extensions(&config, Some(&config_path), |api| {
        api.request_check(
            "private",
            "v1",
            Arc::new(move |input| match checked.lock() {
                Ok(mut rows) => {
                    rows.push(input.clone());
                    Decision::Allow
                }
                Err(_) => Decision::Deny {
                    reason_code: "fixture.lock".into(),
                },
            }),
        )?;
        Ok(())
    })
    .await
}

fn prompt() -> Prompt {
    Prompt {
        model: "bitrouter/private".into(),
        system: None,
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "task")],
        tools: vec![Tool::Function {
            name: "inspect".into(),
            description: None,
            parameters: json!({"type":"object","properties":{"path":{"type":"string"}}}),
            strict: None,
            provider_metadata: Default::default(),
        }],
        params: GenerationParams {
            max_tokens: Some(128),
            ..Default::default()
        },
        response_format: None,
        tool_choice: None,
        stream: false,
    }
}

fn followup(content: Vec<Content>) -> Prompt {
    let calls = content
        .iter()
        .filter_map(|part| match part {
            Content::ToolCall {
                id,
                name: tool_name,
                provider_executed: false,
                ..
            } => Some((id.clone(), tool_name.clone())),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut prompt = prompt();
    prompt.messages.push(Message {
        role: Role::Assistant,
        content,
    });
    for (call_id, tool_name) in calls {
        prompt.messages.push(Message {
            role: Role::Tool,
            content: vec![Content::ToolResult {
                call_id,
                tool_name: Some(tool_name),
                output: ToolResultOutput::Text {
                    value: "observed".into(),
                },
                dynamic: false,
                provider_metadata: Default::default(),
            }],
        });
    }
    prompt.messages.push(Message::text(Role::User, "continue"));
    prompt
}

fn owner() -> CallerContext {
    CallerContext::new("fixture-key-id", "fixture-owner")
}

fn gateway(fixture: &PrivateFixture) -> Result<TestServer> {
    let app = &fixture.app.app;
    TestServer::builder()
        .http_transport()
        .try_build(build_router(AppState {
            language_model: app.language_model().context("missing pipeline")?.clone(),
            mcp: app.mcp().cloned(),
            skip_auth: app.skip_auth(),
            metrics_renderer: app.metrics_renderer().cloned(),
            prompt_transforms: app.prompt_transforms().to_vec(),
        }))
}

#[tokio::test]
async fn responses_reasoning_http_blocks_unprojected_fields_and_preserves_continuation()
-> Result<()> {
    let fixture = PrivateFixture::new("responses").await?;
    let gateway = gateway(&fixture)?;
    let malicious = json!({"model":"bitrouter/private", "input":[
        {"type":"reasoning", "id":"rs_x", "summary":[], "content":[{"type":"reasoning_text", "text":"unchecked sentinel"}]}
    ]});
    let rejected = gateway.post("/v1/responses").json(&malicious).await;
    assert_eq!(rejected.status_code().as_u16(), 400, "{}", rejected.text());
    // A native caller bypasses the HTTP decoder, so the checker boundary must
    // independently validate retained metadata before invoking any extension.
    for part in [
        Content::Reasoning {
            text: String::new(),
            provider_metadata: [("openai".into(), json!({"reasoningItem":malicious["input"][0]}))].into(),
        },
        Content::Reasoning {
            text: "unchecked sentinel".into(),
            provider_metadata: [
                ("openai".into(), json!({"reasoningItem":{"type":"reasoning", "id":"rs_x", "summary":[{"type":"summary_text", "text":"unchecked sentinel"}]}})),
                ("anthropic".into(), json!({"redactedThinking":true})),
            ].into(),
        },
    ] {
        let mut direct = prompt();
        direct.messages.push(Message { role: Role::Assistant, content: vec![part] });
        let error = fixture.app.app.execute_native(direct, owner()).await.err().context("expected projection rejection")?;
        assert!(error.to_string().contains("responses_reasoning_item_invalid"), "{error}");
    }
    assert!(fixture.checks()?.is_empty());
    assert!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests")?
            .is_empty()
    );
    let first = gateway
        .post("/v1/responses")
        .json(&json!({
            "model":"bitrouter/private", "input":"task", "max_output_tokens":128,
            "tools":[{"type":"function", "name":"inspect", "parameters":{"type":"object"}}]
        }))
        .await;
    assert_eq!(first.status_code().as_u16(), 200, "{}", first.text());
    let first: Value = first.json();
    assert!(
        first["id"]
            .as_str()
            .is_some_and(|id| id.starts_with("brc_"))
    );
    assert_eq!(first["output"][0]["encrypted_content"], "signed-fixture");
    assert_eq!(
        first["output"][2]["encrypted_content"],
        "opaque-secret-fixture"
    );
    assert_eq!(first["output"][2]["summary"], json!([]));
    let continued = gateway.post("/v1/responses").json(&json!({
        "model":"bitrouter/private", "previous_response_id":first["id"],
        "input":[{"type":"function_call_output", "call_id":"call_inspect", "output":"observed"}],
        "max_output_tokens":128
    })).await;
    assert_eq!(
        continued.status_code().as_u16(),
        200,
        "{}",
        continued.text()
    );
    let requests = fixture
        .upstream
        .received_requests()
        .await
        .context("requests")?;
    assert_eq!(requests.len(), 2);
    let body: Value = serde_json::from_slice(&requests[1].body)?;
    assert_eq!(body["previous_response_id"], "resp_private");
    assert_eq!(
        body["input"],
        json!([{"type":"function_call_output", "call_id":"call_inspect", "output":"observed"}])
    );
    Ok(())
}

#[tokio::test]
async fn private_history_roundtrip_preserves_parts_ids_receipts_and_checker_scope() -> Result<()> {
    for protocol in ["messages", "generate_content", "responses"] {
        let fixture = PrivateFixture::new(protocol).await?;
        let capture = Arc::new(Capture::default());
        let first = fixture
            .app
            .app
            .execute_native_controlled(prompt(), owner(), capture.clone())
            .await?;
        let first_report = capture.reports.lock().await[0].clone();
        assert_eq!(first_report.result.as_ref(), Some(&first.result));
        assert_eq!(
            first_report.private_context.input,
            PrivateContextEvidence::NotPresent
        );
        assert_eq!(
            first_report.private_context.output,
            PrivateContextEvidence::Verified {
                parts: if protocol == "generate_content" { 5 } else { 2 }
            }
        );
        for part in &first.result.content {
            if let Content::ToolCall { id, .. } = part {
                assert!(!id.is_empty());
            }
        }
        let continuation = followup(first.result.content.clone());
        fixture
            .app
            .app
            .execute_native_controlled(continuation.clone(), owner(), capture.clone())
            .await?;
        let second = capture.reports.lock().await[1].clone();
        assert_eq!(
            second.private_context.input,
            PrivateContextEvidence::Verified {
                parts: if protocol == "generate_content" { 5 } else { 2 }
            }
        );
        let requests = fixture
            .upstream
            .received_requests()
            .await
            .context("missing requests")?;
        assert_eq!(requests.len(), 2);
        let body: Value = serde_json::from_slice(&requests[1].body)?;
        let wire = body.to_string();
        assert!(wire.contains("signed-fixture"));
        assert!(!wire.contains(ORIGIN_FIELD));
        assert!(!wire.contains("fixture-owner"));
        if protocol == "messages" {
            assert_eq!(
                body["messages"][1]["content"][0],
                json!({"type":"thinking","thinking":"readable thought","signature":"signed-fixture"})
            );
            assert_eq!(
                body["messages"][1]["content"][1],
                json!({"type":"redacted_thinking","data":"opaque-secret-fixture"})
            );
            assert_eq!(requests[1].headers["x-api-key"], "fixture-key");
        } else if protocol == "responses" {
            let input = body["input"].as_array().context("Responses input")?;
            let kinds: Vec<_> = input.iter().map(|item| item["type"].as_str()).collect();
            assert_eq!(
                kinds,
                vec![
                    Some("message"),
                    Some("reasoning"),
                    Some("function_call"),
                    Some("reasoning"),
                    Some("message"),
                    Some("function_call_output"),
                    Some("message")
                ]
            );
            assert_eq!(
                input[1],
                json!({"type":"reasoning", "id":"rs_first", "summary":[{"type":"summary_text", "text":"readable thought"}], "encrypted_content":"signed-fixture", "status":"completed"})
            );
            assert_eq!(
                input[3],
                json!({"type":"reasoning", "id":"rs_empty", "summary":[], "encrypted_content":"opaque-secret-fixture"})
            );
            assert_eq!(input[2]["call_id"], "call_inspect");
            assert_eq!(input[5]["call_id"], "call_inspect");
            assert_eq!(input[5]["output"], "observed");
            assert!(body.get("previous_response_id").is_none());
            assert_eq!(requests[1].headers["authorization"], "Bearer fixture-key");
        } else {
            for signature in ["tool", "text", "inline", "file"] {
                assert!(wire.contains(&format!("{signature}-signature-fixture")));
            }
            assert_eq!(body["contents"][1]["parts"][3]["thought"], true);
            assert_eq!(body["contents"][1]["parts"][4]["thought"], true);
            assert_eq!(requests[1].headers["x-goog-api-key"], "fixture-key");
        }
        let checks = fixture.checks()?;
        assert_eq!(checks.len(), 2);
        assert!(!format!("{:?}", checks[1]).contains("opaque-secret-fixture"));
        assert_eq!(
            checks[1].coverage.excluded_private_fragments,
            match protocol {
                "messages" => 1,
                "responses" => 2,
                _ => 0,
            }
        );
        // Recreate the production App: installation provenance survives process-local state.
        let restarted = assemble(&fixture.source, &fixture.home, fixture.checked.clone()).await?;
        restarted
            .app
            .execute_native_controlled(continuation, owner(), Arc::new(Capture::default()))
            .await?;
    }
    Ok(())
}

#[tokio::test]
async fn private_history_tampering_and_owner_switch_stop_before_checkers_or_provider() -> Result<()>
{
    for protocol in ["messages", "generate_content", "responses"] {
        let fixture = PrivateFixture::new(protocol).await?;
        let first = fixture
            .app
            .app
            .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
            .await?;
        let original = followup(first.result.content);
        for case in [
            "proof",
            "payload",
            "position",
            "role",
            "owner",
            "missing",
            "declassified",
            "public_payload",
            "private_payload",
        ] {
            let mut candidate = original.clone();
            let content = &mut candidate.messages[1].content;
            let caller = if case == "owner" {
                CallerContext::new("other-key", "other-owner")
            } else {
                owner()
            };
            match case {
                "proof" => {
                    metadata_mut(&mut content[0])
                        .get_mut(ORIGIN_NAMESPACE)
                        .context("missing namespace")?[ORIGIN_FIELD] = json!("forged");
                }
                "payload" => {
                    if let Content::Reasoning { text, .. } = &mut content[0] {
                        *text = "changed".into();
                    }
                }
                "private_payload" => {
                    let meta = metadata_mut(&mut content[0]);
                    match protocol {
                        "messages" => {
                            meta.get_mut("anthropic").context("metadata")?["signature"] =
                                json!("forged")
                        }
                        "responses" => {
                            meta.get_mut("openai").context("metadata")?["reasoningItem"]["encrypted_content"] =
                                json!("forged")
                        }
                        _ => {
                            meta.get_mut("google").context("metadata")?["thoughtSignature"] =
                                json!("forged")
                        }
                    }
                }
                "public_payload" => {
                    let part = content
                        .iter_mut()
                        .find(|part| matches!(part, Content::Text { .. }))
                        .context("missing ordinary text")?;
                    if let Content::Text { text, .. } = part {
                        *text = "changed public answer".into();
                    }
                }
                "declassified" => {
                    for part in content {
                        metadata_mut(part).remove(if protocol == "messages" {
                            "anthropic"
                        } else if protocol == "responses" {
                            "openai"
                        } else {
                            "google"
                        });
                    }
                }
                "position" => content.swap(0, 1),
                "role" => candidate.messages[1].role = Role::User,
                "missing" => {
                    metadata_mut(&mut content[0]).remove(ORIGIN_NAMESPACE);
                }
                _ => {}
            }
            let result = fixture
                .app
                .app
                .execute_native_controlled(candidate, caller, Arc::new(Capture::default()))
                .await;
            assert!(result.is_err(), "{protocol} {case}");
            assert_eq!(
                fixture.checks()?.len(),
                1,
                "rejected history reached checker: {case}"
            );
            assert_eq!(
                fixture
                    .upstream
                    .received_requests()
                    .await
                    .context("requests")?
                    .len(),
                1
            );
        }
        assert!(original.messages[1].content.iter().any(is_private));
    }
    Ok(())
}

#[tokio::test]
async fn private_history_model_key_and_installation_switch_never_send_generation() -> Result<()> {
    for protocol in ["messages", "generate_content", "responses"] {
        let fixture = PrivateFixture::new(protocol).await?;
        let first = fixture
            .app
            .app
            .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
            .await?;
        let continuation = followup(first.result.content);
        let mut changed_model = continuation.clone();
        changed_model.model = "fixture:changed".into();
        let capture = Arc::new(Capture::default());
        assert!(
            fixture
                .app
                .app
                .execute_native_controlled(changed_model, owner(), capture.clone())
                .await
                .is_err()
        );
        let plans = capture.plans.lock().await;
        assert!(plans[0].routes.iter().all(|route| matches!(&route.protocol_validation,
        NativeProtocolValidation::Rejected { reason } if reason == "private_context_target_mismatch")));
        drop(plans);
        let rotated = assemble(
            &fixture
                .source
                .replace("api_key: fixture-key", "api_key: rotated-key"),
            &fixture.home,
            fixture.checked.clone(),
        )
        .await?;
        let error = rotated
            .app
            .execute_native_controlled(continuation.clone(), owner(), Arc::new(Capture::default()))
            .await
            .err()
            .context("expected auth rejection")?;
        assert!(
            error
                .to_string()
                .contains("private_context_authority_mismatch"),
            "{error}"
        );
        let new_home = tempfile::tempdir()?;
        let foreign = assemble(&fixture.source, &new_home, fixture.checked.clone()).await?;
        let checks_before = fixture.checks()?.len();
        assert!(
            foreign
                .app
                .execute_native_controlled(continuation, owner(), Arc::new(Capture::default()))
                .await
                .is_err()
        );
        assert_eq!(fixture.checks()?.len(), checks_before);
        assert_eq!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("requests")?
                .len(),
            1
        );
    }
    Ok(())
}

#[tokio::test]
async fn private_history_redirect_is_not_followed_or_claimed_as_success() -> Result<()> {
    let fixture = PrivateFixture::new("messages").await?;
    let first = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
        .await?;
    let destination = MockServer::start().await;
    fixture.upstream.reset().await;
    Mock::given(method("POST"))
        .and(path("/messages"))
        .respond_with(
            ResponseTemplate::new(307)
                .insert_header("location", format!("{}/messages", destination.uri())),
        )
        .mount(&fixture.upstream)
        .await;
    let capture = Arc::new(Capture::default());
    assert!(
        fixture
            .app
            .app
            .execute_native_controlled(followup(first.result.content), owner(), capture.clone())
            .await
            .is_err()
    );
    assert!(
        destination
            .received_requests()
            .await
            .context("destination capture")?
            .is_empty()
    );
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("upstream capture")?
            .len(),
        1
    );
    let reports = capture.reports.lock().await;
    assert_eq!(reports.len(), 1);
    assert_eq!(
        reports[0].private_context.input,
        PrivateContextEvidence::Verified { parts: 2 }
    );
    assert_eq!(
        reports[0].private_context.output,
        PrivateContextEvidence::Unknown
    );
    assert!(reports[0].result.is_none());
    Ok(())
}

#[tokio::test]
async fn private_history_core_changes_model_only_after_authorized_whole_message_rebuild()
-> Result<()> {
    use bitrouter_orchestrator::core::protocol::DiscardableHistory;
    for protocol in ["messages", "responses"] {
        for discard in [false, true] {
            let fixture = PrivateFixture::new(protocol).await?;
            if protocol == "responses" {
                fixture.upstream.reset().await;
                Mock::given(method("POST")).and(path("/responses"))
                .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                    "id":"resp_core", "status":"completed", "output":[
                        {"type":"reasoning", "id":"rs_core", "summary":[{"type":"summary_text", "text":"readable thought"}], "encrypted_content":"signed-fixture"},
                        {"type":"reasoning", "id":"rs_core_empty", "summary":[], "encrypted_content":"opaque-secret-fixture"},
                        {"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"done"}]}
                    ], "usage":{"input_tokens":12,"output_tokens":5,"total_tokens":17}
                }))).mount(&fixture.upstream).await;
            }
            let grant = OwnershipGrant {
                session_id: "private-session".into(),
                harness_id: "private-harness".into(),
                core_instance_id: "private-core".into(),
                execution_epoch: 1,
            };
            let harness = Arc::new(crate::Harness {
                grant: grant.clone(),
                store: Mutex::new(crate::Store::default()),
            });
            let manifest = HarnessManifest {
                tool_manifest_digest: HarnessManifest::digest(&[])?,
                tools: Vec::new(),
                workspace_id: "workspace".into(),
                workspace_revision: None,
                permission_revision: 1,
                max_tool_output_bytes: 8192,
                artifact_quota_bytes: 1024 * 1024,
                max_artifact_chunk_bytes: 8192,
                required_features: Vec::new(),
            };
            let caps = Capabilities {
                version: 1,
                core_instance_id: "private-core".into(),
                operations: Vec::new(),
                transports: vec!["in_process".into()],
                unsupported_features: Vec::new(),
                limits: Limits::default(),
                max_sessions: 16,
                max_host_model_attempts: 16,
            };
            let session = CoreSession::bind(
                Bind {
                    grant,
                    durable_head: DurableHead::default(),
                    checkpoint: None,
                    manifest: manifest.clone(),
                    limits: Limits::default(),
                },
                &caps,
                Arc::new(fixture.app.app),
                owner(),
                harness.clone(),
            )
            .await?;
            session
                .signals(
                    "materials",
                    crate::SignalUpdate {
                        signal_revision: 1,
                        observed_at: "2026-10-02T12:00:00Z".into(),
                        scope: "private-session".into(),
                        source: "private-harness".into(),
                        workspace_revision: None,
                        manifest,
                        facts: Default::default(),
                        materials: vec![crate::MaterialRef {
                            material_id: "source".into(),
                            version: "v1".into(),
                            sha256: crate::sha256(b"required private test source"),
                            media_type: "text/plain".into(),
                            provenance: "harness_document".into(),
                            required: true,
                            artifact: None,
                            content: Some("required private test source".into()),
                        }],
                    },
                )
                .await?;
            let mut initial = crate::input("initial task");
            initial.model = "bitrouter/private".into();
            session
                .start("first", session.head().await.state_revision, initial)
                .await?;
            let first = session.drive().await?;
            assert_eq!(
                first.run.as_ref().map(|run| run.status),
                Some(RunStatus::Completed)
            );
            let history = first.agents[&first.agent_id].history.clone();
            let private_message = history
                .iter()
                .find(|message| message.content.iter().any(is_private))
                .context("missing private history")?;
            let receipt = first.root_turn().context("turn")?.steps[0].attempts[0]
                .receipt
                .as_ref()
                .context("receipt")?;
            assert_eq!(
                &receipt.report.result.as_ref().context("result")?.content,
                &private_message.content
            );
            assert_eq!(
                receipt.report.private_context.output,
                PrivateContextEvidence::Verified { parts: 2 }
            );
            let mut next = crate::input("independent next task");
            next.model = "fixture:changed".into();
            if discard {
                next.discardable_history = Some(DiscardableHistory {
                    history_sha256: crate::sha256(&serde_json::to_vec(&history)?),
                    message_indices: history
                        .iter()
                        .enumerate()
                        .filter_map(|(index, message)| {
                            (message.role == Role::Assistant).then_some(index)
                        })
                        .collect(),
                });
            }
            session
                .start("second", session.head().await.state_revision, next)
                .await?;
            let second = session.drive().await?;
            let requests = fixture
                .upstream
                .received_requests()
                .await
                .context("wire requests")?;
            assert_eq!(requests.len(), if discard { 2 } else { 1 });
            let turn = second.root_turn().context("second turn")?;
            assert!(turn.steps[0].attempts.is_empty());
            if discard {
                assert_eq!(
                    second.run.as_ref().map(|run| run.status),
                    Some(RunStatus::Completed)
                );
                assert_eq!(turn.steps.len(), 2);
                let sent = std::str::from_utf8(&requests[1].body)?;
                assert!(!sent.contains("signed-fixture"));
                assert!(!sent.contains("opaque-secret-fixture"));
                assert!(sent.contains("independent next task"));
                let second_receipt = turn.steps[1].attempts[0]
                    .receipt
                    .as_ref()
                    .context("second receipt")?;
                assert_eq!(
                    second_receipt.report.private_context.input,
                    PrivateContextEvidence::NotPresent
                );
                assert_eq!(
                    second_receipt.report.private_context.output,
                    PrivateContextEvidence::Verified { parts: 2 }
                );
            } else {
                assert_ne!(
                    second.run.as_ref().map(|run| run.status),
                    Some(RunStatus::Completed)
                );
            }
            assert!(!harness.store.lock().await.batches.is_empty());
        }
    }
    Ok(())
}

#[tokio::test]
async fn private_history_seal_failure_retains_real_output_usage_and_settlement() -> Result<()> {
    use bitrouter_sdk::language_model::native_context::{PrivateContextFailure, metadata};
    let fixture = PrivateFixture::new("messages").await?;
    std::fs::write(fixture.home.path().join("continuation.key"), "invalid-key")?;
    let capture = Arc::new(Capture::default());
    let response = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), capture.clone())
        .await?;
    let usage = response.result.usage.as_ref().context("missing usage")?;
    assert_eq!(usage.prompt_tokens, 12);
    assert_eq!(usage.completion_tokens, 5);
    assert_eq!(response.result.content.len(), 3);
    assert!(
        response
            .result
            .content
            .iter()
            .all(|part| !metadata(part).contains_key(ORIGIN_NAMESPACE))
    );
    let report = capture.reports.lock().await[0].clone();
    assert_eq!(report.result.as_ref(), Some(&response.result));
    assert_eq!(
        report.private_context.output,
        PrivateContextEvidence::Unverified {
            reason: PrivateContextFailure::KeyUnavailable
        }
    );
    fixture
        .app
        .app
        .language_model()
        .context("pipeline")?
        .drain_required_pending_settlements()
        .await?;
    let rows = requests::Entity::find().all(&fixture.app.db).await?;
    assert_eq!(rows.len(), 1);
    assert!(rows[0].error.is_none());
    assert!(rows[0].raw_usage_json.is_some());
    assert!(
        fixture
            .app
            .app
            .execute_native_controlled(
                followup(response.result.content),
                owner(),
                Arc::new(Capture::default())
            )
            .await
            .is_err()
    );
    assert_eq!(fixture.checks()?.len(), 1);
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests")?
            .len(),
        1
    );
    Ok(())
}
