//! Native continuation through the production App, authentication and HTTP executor.
//! https://developers.openai.com/api/docs/guides/conversation-state

#[path = "continuation/collaboration.rs"]
mod collaboration;

use super::*;
use bitrouter_sdk::language_model::native_context::metadata;
use bitrouter_sdk::language_model::native_continuation::{
    CONTINUATION_FIELD, FullHistoryReason, NativeContinuationInput, NativeContinuationOutput,
    REQUIRED_STATE_FIELD,
};

async fn stored_fixture(private: bool, replayable: bool, counting: bool) -> Result<PrivateFixture> {
    let mut fixture = PrivateFixture::new("responses").await?;
    fixture.upstream.reset().await;
    Mock::given(method("POST")).and(path("/responses"))
        .respond_with(move |request: &wiremock::Request| {
            let previous = serde_json::from_slice::<Value>(&request.body).ok()
                .and_then(|body| body.get("previous_response_id").cloned());
            let (id, text) = if previous.is_some() { ("resp_native_second", "second answer") } else { ("resp_native_first", "first answer") };
            let mut output = Vec::new();
            if private { output.push(json!({"type":"reasoning", "id":"rs_native", "summary":[], "encrypted_content":"opaque-native-reasoning"})); }
            if !replayable { output.push(json!({"type":"unrepresented_fixture_item", "opaque":"hidden-state"})); }
            output.push(json!({"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":text}]}));
            ResponseTemplate::new(200).set_body_json(json!({
                "id":id, "status":"completed", "store":true, "output":output,
                "usage":{"input_tokens":100,"output_tokens":5,"total_tokens":105}
            }))
        }).mount(&fixture.upstream).await;
    if counting {
        Mock::given(method("POST"))
            .and(path("/responses/input_tokens"))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"object":"response.input_tokens","input_tokens":100})),
            )
            .mount(&fixture.upstream)
            .await;
        fixture.source = fixture.source.replace(
            "api_protocol: responses",
            "api_protocol: responses\n        input_token_counting: responses",
        );
        fixture.app = assemble(&fixture.source, &fixture.home, fixture.checked.clone()).await?;
    }
    Ok(fixture)
}

fn suffix_prompt(content: Vec<Content>) -> Prompt {
    let mut next = followup(content);
    next.system = Some("Current instructions are sent on every request.".into());
    next
}

fn artifact(content: &[Content]) -> Result<&str> {
    content
        .first()
        .and_then(|part| metadata(part).get(ORIGIN_NAMESPACE))
        .and_then(|fields| fields.get(CONTINUATION_FIELD))
        .and_then(Value::as_str)
        .context("missing native continuation artifact")
}

#[tokio::test]
async fn native_continuation_uses_suffix_and_same_count_body_after_restart() -> Result<()> {
    for private in [false, true] {
        let fixture = stored_fixture(private, true, true).await?;
        let capture = Arc::new(Capture::default());
        let first = fixture
            .app
            .app
            .execute_native_controlled(prompt(), owner(), capture.clone())
            .await?;
        assert_eq!(
            capture.reports.lock().await[0].continuation.output,
            NativeContinuationOutput::Issued
        );
        assert!(first.result.response_id.is_none());
        assert!(
            !serde_json::to_string(&capture.reports.lock().await[0])?.contains("resp_native_first")
        );
        let token = artifact(&first.result.content)?;
        assert!(!token.contains("resp_native_first"));
        assert!(!token.contains("fixture-owner"));
        let next = suffix_prompt(first.result.content);
        let restarted = assemble(&fixture.source, &fixture.home, fixture.checked.clone()).await?;
        let continued = restarted
            .app
            .execute_native_controlled(next.clone(), owner(), capture.clone())
            .await?;
        let plans = capture.plans.lock().await;
        assert_eq!(plans[1].prompt, next);
        assert_eq!(
            plans[1].routes[0].continuation,
            NativeContinuationInput::Resumed { prefix_messages: 2 }
        );
        drop(plans);
        let reports = capture.reports.lock().await;
        assert_eq!(reports[1].result.as_ref(), Some(&continued.result));
        assert_eq!(
            reports[1].continuation.input,
            NativeContinuationInput::Resumed { prefix_messages: 2 }
        );
        assert_eq!(
            reports[1].continuation.output,
            NativeContinuationOutput::Issued
        );
        assert_eq!(
            reports[1].private_context.input,
            PrivateContextEvidence::NotPresent
        );
        drop(reports);
        let requests = fixture
            .upstream
            .received_requests()
            .await
            .context("requests")?;
        assert_eq!(requests.len(), 4);
        let count: Value = serde_json::from_slice(&requests[2].body)?;
        let generation: Value = serde_json::from_slice(&requests[3].body)?;
        assert_eq!(requests[2].url.path(), "/responses/input_tokens");
        assert_eq!(requests[3].url.path(), "/responses");
        assert_eq!(generation["previous_response_id"], "resp_native_first");
        assert_eq!(
            generation["input"],
            json!([{"type":"message", "role":"user", "content":[{"type":"input_text", "text":"continue"}]}])
        );
        assert_eq!(count["input"], generation["input"]);
        assert_eq!(
            count["previous_response_id"],
            generation["previous_response_id"]
        );
        assert_eq!(count["instructions"], generation["instructions"]);
        assert_eq!(
            generation["instructions"],
            "Current instructions are sent on every request."
        );
        assert!(!generation.to_string().contains(CONTINUATION_FIELD));
        assert!(!generation.to_string().contains(ORIGIN_FIELD));
        assert!(!generation.to_string().contains("opaque-native-reasoning"));
        assert!(!generation.to_string().contains("first answer"));
        assert_eq!(requests[3].headers["authorization"], "Bearer fixture-key");
    }
    Ok(())
}

#[tokio::test]
async fn native_continuation_rejects_tampering_before_checker_or_upstream() -> Result<()> {
    let fixture = stored_fixture(false, true, false).await?;
    let first = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
        .await?;
    let original = suffix_prompt(first.result.content);
    for case in ["cipher", "message", "owner", "role", "position"] {
        let mut next = original.clone();
        let mut caller = owner();
        match case {
            "cipher" => {
                metadata_mut(&mut next.messages[1].content[0])
                    .get_mut(ORIGIN_NAMESPACE)
                    .context("metadata")?[CONTINUATION_FIELD] = json!("forged")
            }
            "message" => {
                if let Content::Text { text, .. } = &mut next.messages[1].content[0] {
                    *text = "changed".into();
                }
            }
            "owner" => caller = CallerContext::new("other", "other-owner"),
            "role" => next.messages[1].role = Role::User,
            "position" => next.messages[1].content.insert(
                0,
                Content::Text {
                    text: "inserted".into(),
                    provider_metadata: Default::default(),
                },
            ),
            _ => {}
        }
        assert!(
            fixture
                .app
                .app
                .execute_native_controlled(next, caller, Arc::new(Capture::default()))
                .await
                .is_err(),
            "{case}"
        );
        assert_eq!(fixture.checks()?.len(), 1, "{case}");
        assert_eq!(
            fixture
                .upstream
                .received_requests()
                .await
                .context("requests")?
                .len(),
            1,
            "{case}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn native_continuation_detaches_only_when_complete_replay_is_available() -> Result<()> {
    for replayable in [true, false] {
        let fixture = stored_fixture(false, replayable, false).await?;
        let first = fixture
            .app
            .app
            .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
            .await?;
        let next = suffix_prompt(first.result.content);
        for (case, reason) in [
            ("model", FullHistoryReason::TargetChanged),
            ("effort", FullHistoryReason::EffortChanged),
            ("prefix", FullHistoryReason::PrefixChanged),
        ] {
            let mut changed = next.clone();
            match case {
                "model" => changed.model = "fixture:changed".into(),
                "effort" => {
                    changed.params.reasoning_effort = Some(ReasoningEffort::High);
                    changed.params.reasoning_effort_source = ReasoningEffortSource::Caller;
                }
                "prefix" => changed.messages[0] = Message::text(Role::User, "edited prefix"),
                _ => {}
            }
            let before = fixture
                .upstream
                .received_requests()
                .await
                .context("requests")?
                .len();
            let capture = Arc::new(Capture::default());
            let result = fixture
                .app
                .app
                .execute_native_controlled(changed, owner(), capture.clone())
                .await;
            let requests = fixture
                .upstream
                .received_requests()
                .await
                .context("requests")?;
            if replayable {
                result?;
                assert_eq!(requests.len(), before + 1);
                let wire: Value = serde_json::from_slice(&requests[before].body)?;
                assert!(wire.get("previous_response_id").is_none());
                assert!(wire.to_string().contains("first answer"));
                let reports = capture.reports.lock().await;
                assert_eq!(
                    reports[0].continuation.input,
                    NativeContinuationInput::FullHistory { reason }
                );
            } else {
                assert!(result.is_err(), "{case}");
                assert_eq!(requests.len(), before);
                let plans = capture.plans.lock().await;
                assert!(matches!(
                    plans[0].routes[0].continuation,
                    NativeContinuationInput::Rejected { .. }
                ));
            }
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_continuation_key_rotation_and_unstored_responses_never_reuse_handles() -> Result<()>
{
    let fixture = stored_fixture(false, true, false).await?;
    let first = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
        .await?;
    let next = suffix_prompt(first.result.content);
    let rotated = assemble(
        &fixture
            .source
            .replace("api_key: fixture-key", "api_key: changed-key"),
        &fixture.home,
        fixture.checked.clone(),
    )
    .await?;
    let error = rotated
        .app
        .execute_native_controlled(next.clone(), owner(), Arc::new(Capture::default()))
        .await
        .err()
        .context("expected authority rejection")?;
    assert!(
        error
            .to_string()
            .contains("native_continuation_authority_mismatch"),
        "{error}"
    );
    let foreign_home = tempfile::tempdir()?;
    let foreign = assemble(&fixture.source, &foreign_home, fixture.checked.clone()).await?;
    assert!(
        foreign
            .app
            .execute_native_controlled(next, owner(), Arc::new(Capture::default()))
            .await
            .is_err()
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
    let mut not_stored = prompt();
    not_stored.params.store = Some(false);
    let capture = Arc::new(Capture::default());
    let response = fixture
        .app
        .app
        .execute_native_controlled(not_stored, owner(), capture.clone())
        .await?;
    assert!(artifact(&response.result.content).is_err());
    assert_eq!(
        capture.reports.lock().await[0].continuation.output,
        NativeContinuationOutput::NotStored
    );
    assert_eq!(
        response
            .result
            .usage
            .as_ref()
            .context("usage")?
            .prompt_tokens,
        100
    );
    Ok(())
}

fn remove_field(part: &mut Content, field: &str) -> Result<()> {
    metadata_mut(part)
        .get_mut(ORIGIN_NAMESPACE)
        .and_then(Value::as_object_mut)
        .context("metadata fields")?
        .remove(field);
    Ok(())
}

#[tokio::test]
async fn native_continuation_never_replays_unrepresented_state_without_coverage() -> Result<()> {
    let fixture = stored_fixture(false, false, false).await?;
    let first = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
        .await?;
    let mut next = suffix_prompt(first.result.content);
    assert!(
        metadata(&next.messages[1].content[0])[ORIGIN_NAMESPACE][REQUIRED_STATE_FIELD].is_string()
    );
    let second = fixture
        .app
        .app
        .execute_native_controlled(next.clone(), owner(), Arc::new(Capture::default()))
        .await?;
    // Deleting just the newest handle must not fall back to the older anchor.
    let mut older = next.clone();
    older.messages.push(Message {
        role: Role::Assistant,
        content: second.result.content,
    });
    older.messages.push(Message::text(Role::User, "third task"));
    remove_field(&mut older.messages[3].content[0], CONTINUATION_FIELD)?;
    remove_field(&mut next.messages[1].content[0], CONTINUATION_FIELD)?;
    for prompt in [next.clone(), older] {
        let capture = Arc::new(Capture::default());
        assert!(
            fixture
                .app
                .app
                .execute_native_controlled(prompt, owner(), capture.clone())
                .await
                .is_err()
        );
        assert_eq!(capture.plans.lock().await[0].routes[0].continuation,
            NativeContinuationInput::Rejected { reason: bitrouter_sdk::language_model::native_continuation::ContinuationFailure::Required });
    }
    // Removing the state requirement while leaving its proof invalidates the
    // whole assistant message, before a checker sees the edited input.
    remove_field(&mut next.messages[1].content[0], REQUIRED_STATE_FIELD)?;
    let checks = fixture.checks()?.len();
    let error = fixture
        .app
        .app
        .execute_native_controlled(next, owner(), Arc::new(Capture::default()))
        .await
        .err()
        .context("expected proof failure")?;
    assert!(
        error.to_string().contains("private_context_proof_invalid"),
        "{error}"
    );
    assert_eq!(fixture.checks()?.len(), checks);
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests")?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn native_continuation_cannot_detach_a_lossy_imported_branch() -> Result<()> {
    let fixture = stored_fixture(false, false, false).await?;
    let lossy = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
        .await?;
    fixture.upstream.reset().await;
    Mock::given(method("POST")).and(path("/responses"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id":"resp_independent", "status":"completed", "store":true,
            "output":[{"type":"message", "role":"assistant", "content":[{"type":"output_text", "text":"independent answer"}]}],
            "usage":{"input_tokens":100,"output_tokens":5,"total_tokens":105}
        }))).mount(&fixture.upstream).await;
    let complete = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
        .await?;
    let mut joined = suffix_prompt(lossy.result.content);
    joined.messages.push(Message {
        role: Role::Assistant,
        content: complete.result.content,
    });
    joined
        .messages
        .push(Message::text(Role::User, "joined task"));
    assert!(
        fixture
            .app
            .app
            .execute_native_controlled(joined, owner(), Arc::new(Capture::default()))
            .await
            .is_err()
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
    Ok(())
}

#[tokio::test]
async fn native_continuation_identical_visible_outputs_cannot_exchange_hidden_state() -> Result<()>
{
    let fixture = stored_fixture(false, false, false).await?;
    let first = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
        .await?;
    let second = fixture
        .app
        .app
        .execute_native_controlled(prompt(), owner(), Arc::new(Capture::default()))
        .await?;
    let second_token = artifact(&second.result.content)?.to_owned();
    let mut next = suffix_prompt(first.result.content);
    metadata_mut(&mut next.messages[1].content[0])
        .get_mut(ORIGIN_NAMESPACE)
        .context("metadata")?[CONTINUATION_FIELD] = second_token.into();
    let error = fixture
        .app
        .app
        .execute_native_controlled(next, owner(), Arc::new(Capture::default()))
        .await
        .err()
        .context("expected artifact failure")?;
    assert!(
        error
            .to_string()
            .contains("native_continuation_artifact_invalid"),
        "{error}"
    );
    assert_eq!(fixture.checks()?.len(), 2);
    assert_eq!(
        fixture
            .upstream
            .received_requests()
            .await
            .context("requests")?
            .len(),
        2
    );
    Ok(())
}

#[tokio::test]
async fn native_continuation_core_followups_persist_full_history_and_send_only_suffix() -> Result<()>
{
    let fixture = stored_fixture(true, false, false).await?;
    let grant = OwnershipGrant {
        session_id: "native-session".into(),
        harness_id: "native-harness".into(),
        core_instance_id: "native-core".into(),
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
        core_instance_id: "native-core".into(),
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
            manifest,
            limits: Limits::default(),
        },
        &caps,
        Arc::new(fixture.app.app),
        owner(),
        harness.clone(),
    )
    .await?;
    let mut previous_history = Vec::new();
    for (index, text) in ["first task", "second task", "third task"]
        .into_iter()
        .enumerate()
    {
        let mut input = crate::input(text);
        input.model = "bitrouter/private".into();
        session
            .start(
                &format!("turn-{index}"),
                session.head().await.state_revision,
                input,
            )
            .await?;
        let snapshot = session.drive().await?;
        assert_eq!(
            snapshot.run.as_ref().map(|run| run.status),
            Some(RunStatus::Completed)
        );
        let history = &snapshot.agents[&snapshot.agent_id].history;
        assert!(history.starts_with(&previous_history));
        let receipt = snapshot.root_turn().context("turn")?.steps[0].attempts[0]
            .receipt
            .as_ref()
            .context("receipt")?;
        assert_eq!(
            &receipt.report.result.as_ref().context("result")?.content,
            &history.last().context("assistant")?.content
        );
        assert_eq!(
            receipt.report.continuation.output,
            NativeContinuationOutput::Issued
        );
        if index > 0 {
            assert_eq!(
                receipt.report.continuation.input,
                NativeContinuationInput::Resumed {
                    prefix_messages: previous_history.len() as u64
                }
            );
        }
        let encoded = serde_json::to_string(&snapshot)?;
        assert!(!encoded.contains("resp_native_first"));
        assert!(!encoded.contains("resp_native_second"));
        previous_history = history.clone();
    }
    let requests = fixture
        .upstream
        .received_requests()
        .await
        .context("requests")?;
    assert_eq!(requests.len(), 3);
    for (index, request) in requests.iter().enumerate().skip(1) {
        let wire: Value = serde_json::from_slice(&request.body)?;
        assert_eq!(
            wire["previous_response_id"],
            if index == 1 {
                "resp_native_first"
            } else {
                "resp_native_second"
            }
        );
        assert!(!wire["input"].to_string().contains("first task"));
        assert!(!wire.to_string().contains("opaque-native-reasoning"));
        assert!(!wire.to_string().contains(CONTINUATION_FIELD));
    }
    let store = harness.store.lock().await;
    assert!(!store.batches.is_empty());
    let payloads = store
        .batches
        .iter()
        .map(|batch| batch.decode(&Limits::default()))
        .collect::<std::result::Result<Vec<_>, _>>()?;
    let serialized = serde_json::to_string(&payloads)?;
    assert!(!serialized.contains("resp_native_first"));
    assert!(!serialized.contains("resp_native_second"));
    assert!(serialized.contains(CONTINUATION_FIELD));
    Ok(())
}

#[tokio::test]
async fn native_continuation_failed_seal_or_storage_preserves_billed_output_and_blocks_lossy_replay()
-> Result<()> {
    for key_failure in [false, true] {
        let fixture = stored_fixture(false, false, false).await?;
        let mut input = prompt();
        if key_failure {
            std::fs::write(fixture.home.path().join("continuation.key"), "invalid-key")?;
        } else {
            input.params.store = Some(false);
        }
        let capture = Arc::new(Capture::default());
        let output = fixture
            .app
            .app
            .execute_native_controlled(input, owner(), capture.clone())
            .await?;
        assert!(artifact(&output.result.content).is_err());
        assert!(output.result.response_id.is_none());
        assert_eq!(
            output.result.usage.as_ref().context("usage")?.prompt_tokens,
            100
        );
        assert!(
            bitrouter_sdk::language_model::native_continuation::requires_stored_state(
                &output.result.content[0]
            )
        );
        let report = &capture.reports.lock().await[0];
        assert_eq!(report.result.as_ref(), Some(&output.result));
        assert_eq!(
            report.continuation.output,
            if key_failure {
                NativeContinuationOutput::Unverified { reason: bitrouter_sdk::language_model::native_continuation::ContinuationFailure::KeyUnavailable }
            } else {
                NativeContinuationOutput::NotStored
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
                    suffix_prompt(output.result.content),
                    owner(),
                    Arc::new(Capture::default())
                )
                .await
                .is_err()
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
    Ok(())
}
