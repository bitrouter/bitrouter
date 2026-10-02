//! Production private-history proof and wire round trips. Provider fixtures are
//! authored independently of the gateway renderers.
//! https://platform.claude.com/docs/en/build-with-claude/extended-thinking
//! https://ai.google.dev/gemini-api/docs/thought-signatures

use super::*;
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

#[tokio::test]
async fn private_history_roundtrip_preserves_parts_ids_receipts_and_checker_scope() -> Result<()> {
    for protocol in ["messages", "generate_content"] {
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
                parts: if protocol == "messages" { 2 } else { 5 }
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
                parts: if protocol == "messages" { 2 } else { 5 }
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
            u64::from(protocol == "messages")
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
    for protocol in ["messages", "generate_content"] {
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
    let fixture = PrivateFixture::new("messages").await?;
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
    for discard in [false, true] {
        let fixture = PrivateFixture::new("messages").await?;
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
