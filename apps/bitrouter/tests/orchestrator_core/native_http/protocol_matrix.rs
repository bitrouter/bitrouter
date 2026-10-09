//! Cross-entry conformance for the common function-tool request subset.
//! Rich Responses controls and fallback accounting live in the parent module.

use super::*;

fn tool_schema() -> Value {
    json!({"type":"object", "properties":{"path":{"type":"string"}}, "required":["path"]})
}

fn native() -> Prompt {
    Prompt {
        model: "fixture:served-model".into(),
        system: Some("matrix-system".into()),
        system_provider_metadata: Default::default(),
        messages: vec![Message::text(Role::User, "matrix-task")],
        tools: vec![Tool::Function {
            name: "inspect".into(),
            description: Some("Inspect evidence".into()),
            parameters: tool_schema(),
            strict: None,
            provider_metadata: Default::default(),
        }],
        params: GenerationParams {
            temperature: Some(0.7),
            top_p: Some(0.9),
            max_tokens: Some(128),
            ..Default::default()
        },
        response_format: None,
        tool_choice: Some(ToolChoice::Auto),
        stream: false,
    }
}

// Independent wire fixtures for the common supported request subset.
// https://developers.openai.com/api/reference/resources/responses/methods/create
// https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create
// https://platform.claude.com/docs/en/api/messages
fn inbound_requests() -> [(&'static str, Value); 3] {
    [
        (
            "/v1/responses",
            json!({
                "model":"fixture:served-model", "instructions":"matrix-system", "input":"matrix-task",
                "temperature":0.7, "top_p":0.9, "max_output_tokens":128,
                "tools":[{"type":"function", "name":"inspect", "description":"Inspect evidence", "parameters":tool_schema()}],
                "tool_choice":"auto", "stream":false
            }),
        ),
        (
            "/v1/chat/completions",
            json!({
                "model":"fixture:served-model", "messages":[{"role":"system", "content":"matrix-system"}, {"role":"user", "content":"matrix-task"}],
                "temperature":0.7, "top_p":0.9, "max_tokens":128,
                "tools":[{"type":"function", "function":{"name":"inspect", "description":"Inspect evidence", "parameters":tool_schema()}}],
                "tool_choice":"auto", "stream":false
            }),
        ),
        (
            "/v1/messages",
            json!({
                "model":"fixture:served-model", "system":"matrix-system", "messages":[{"role":"user", "content":"matrix-task"}],
                "temperature":0.7, "top_p":0.9, "max_tokens":128,
                "tools":[{"name":"inspect", "description":"Inspect evidence", "input_schema":tool_schema()}],
                "tool_choice":{"type":"auto"}, "stream":false
            }),
        ),
    ]
}

fn upstreams() -> [(&'static str, &'static str, Value); 3] {
    [
        (
            "responses",
            "/responses",
            json!({
                "id":"resp_matrix", "object":"response", "status":"completed", "model":"served-model",
                "output":[{"id":"msg_matrix", "type":"message", "role":"assistant", "status":"completed",
                    "content":[{"type":"output_text", "text":"matrix-done", "annotations":[]}]}],
                "usage":{"input_tokens":10, "output_tokens":3, "total_tokens":13}
            }),
        ),
        (
            "chat_completions",
            "/chat/completions",
            json!({
                "id":"chatcmpl_matrix", "object":"chat.completion", "model":"served-model",
                "choices":[{"index":0, "message":{"role":"assistant", "content":"matrix-done"}, "finish_reason":"stop"}],
                "usage":{"prompt_tokens":10, "completion_tokens":3, "total_tokens":13}
            }),
        ),
        (
            "messages",
            "/messages",
            json!({
                "id":"msg_matrix", "type":"message", "role":"assistant", "model":"served-model",
                "content":[{"type":"text", "text":"matrix-done"}], "stop_reason":"end_turn",
                "usage":{"input_tokens":10, "output_tokens":3}
            }),
        ),
    ]
}

fn assert_preserved(body: &Value, protocol: &str) -> Result<()> {
    let inbound = inbound_requests();
    let (expected_tools, expected_messages) = match protocol {
        "responses" => (
            &inbound[0].1["tools"],
            json!([{"type":"message", "role":"user", "content":[{"type":"input_text", "text":"matrix-task"}]}]),
        ),
        "chat_completions" => (&inbound[1].1["tools"], inbound[1].1["messages"].clone()),
        "messages" => (
            &inbound[2].1["tools"],
            json!([{"role":"user", "content":[{"type":"text", "text":"matrix-task"}]}]),
        ),
        _ => anyhow::bail!("unsupported protocol: {protocol}"),
    };
    assert_eq!(&body["tools"], expected_tools);
    let message_key = match protocol {
        "responses" => "input",
        _ => "messages",
    };
    assert_eq!(body[message_key], expected_messages);
    assert_eq!(body["stream"], false);
    let (params, token_key, top_p_key, system, user, tool, schema_key, choice) = match protocol {
        "responses" => (
            body,
            "max_output_tokens",
            "top_p",
            &body["instructions"],
            &body["input"][0]["content"][0]["text"],
            &body["tools"][0],
            "parameters",
            json!("auto"),
        ),
        "chat_completions" => (
            body,
            "max_tokens",
            "top_p",
            &body["messages"][0]["content"],
            &body["messages"][1]["content"],
            &body["tools"][0]["function"],
            "parameters",
            json!("auto"),
        ),
        "messages" => (
            body,
            "max_tokens",
            "top_p",
            &body["system"],
            &body["messages"][0]["content"][0]["text"],
            &body["tools"][0],
            "input_schema",
            json!({"type":"auto"}),
        ),
        _ => (
            &body["generationConfig"],
            "maxOutputTokens",
            "topP",
            &body["systemInstruction"]["parts"][0]["text"],
            &body["contents"][0]["parts"][0]["text"],
            &body["tools"][0]["functionDeclarations"][0],
            "parameters",
            json!({"functionCallingConfig":{"mode":"AUTO"}}),
        ),
    };
    assert_eq!(params["temperature"], 0.7);
    assert_eq!(params[top_p_key], 0.9);
    assert_eq!(params[token_key], 128);
    assert_eq!(system, "matrix-system");
    assert_eq!(user, "matrix-task");
    assert_eq!(tool["name"], "inspect");
    assert_eq!(tool["description"], "Inspect evidence");
    assert_eq!(tool[schema_key], tool_schema());
    assert_eq!(body["model"], "served-model");
    assert_eq!(body["tool_choice"], choice);
    Ok(())
}

fn assert_response(body: &Value, path: &str) {
    let (text, input, output) = match path {
        "/v1/responses" => (
            &body["output"][0]["content"][0]["text"],
            &body["usage"]["input_tokens"],
            &body["usage"]["output_tokens"],
        ),
        "/v1/chat/completions" => (
            &body["choices"][0]["message"]["content"],
            &body["usage"]["prompt_tokens"],
            &body["usage"]["completion_tokens"],
        ),
        "/v1/messages" => (
            &body["content"][0]["text"],
            &body["usage"]["input_tokens"],
            &body["usage"]["output_tokens"],
        ),
        _ => (
            &body["candidates"][0]["content"]["parts"][0]["text"],
            &body["usageMetadata"]["promptTokenCount"],
            &body["usageMetadata"]["candidatesTokenCount"],
        ),
    };
    assert_eq!(text, "matrix-done");
    assert_eq!(input, 10);
    assert_eq!(output, 3);
}

#[tokio::test]
async fn common_native_constraints_and_accounting_match_three_by_three_http_protocols() -> Result<()>
{
    assert_no_runtime_state_in_repository();
    for (outbound, upstream_path, output) in upstreams() {
        let upstream = MockServer::start().await;
        Mock::given(method("POST"))
            .and(path(upstream_path))
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
    api_key: fixture-matrix
    models:
      - id: served-model
        api_protocol: {outbound}
        capabilities: [tools]
        pricing:
          input_micro_usd_per_token: 2
          output_micro_usd_per_token: 4
"#,
            upstream.uri()
        );
        let config = bitrouter_sdk::config::parse_with(&source, |_| None)?;
        let runtime_home = tempfile::tempdir()?;
        let config_path = runtime_home.path().join("bitrouter.yaml");
        std::fs::write(&config_path, source)?;
        let assembled =
            bitrouter::assemble::build_app_with_path(&config, Some(&config_path)).await?;
        let gateway = TestServer::builder()
            .http_transport()
            .try_build(build_router(AppState {
                language_model: assembled
                    .app
                    .language_model()
                    .context("missing pipeline")?
                    .clone(),
                mcp: assembled.app.mcp().cloned(),
                skip_auth: assembled.app.skip_auth(),
                metrics_renderer: assembled.app.metrics_renderer().cloned(),
                prompt_transforms: assembled.app.prompt_transforms().to_vec(),
            }))?;
        for (index, (inbound, body)) in inbound_requests().into_iter().enumerate() {
            let capture = Arc::new(Capture::default());
            let native = assembled
                .app
                .execute_native_controlled(native(), CallerContext::local(), capture.clone())
                .await
                .with_context(|| format!("native {outbound}"))?;
            let http = gateway.post(inbound).json(&body).await;
            assert_eq!(
                http.status_code().as_u16(),
                200,
                "{inbound} -> {outbound}: {}",
                http.text()
            );
            assert_response(&http.json(), inbound);
            assert_eq!(
                generation(&native.result)?.content,
                vec![bitrouter_ai::types::Content::Text {
                    text: "matrix-done".into(),
                    provider_metadata: Default::default(),
                }]
            );
            let http_id = http.header("x-bitrouter-request-id").to_str()?.to_owned();
            assert_ne!(http_id, native.request_id);
            let requests = upstream
                .received_requests()
                .await
                .context("missing upstream capture")?;
            assert_eq!(requests.len(), (index + 1) * 2);
            let pair = &requests[index * 2..index * 2 + 2];
            let native_body: Value = serde_json::from_slice(&pair[0].body)?;
            let http_body: Value = serde_json::from_slice(&pair[1].body)?;
            assert_eq!(native_body, http_body, "{inbound} -> {outbound}");
            assert_eq!(pair[0].url.path(), upstream_path);
            assert_eq!(pair[1].url.path(), upstream_path);
            assert_preserved(&native_body, outbound)?;
            let reports = capture.reports.lock().await;
            assert_eq!(reports.len(), 1);
            let report = &reports[0];
            assert_eq!(report.actual_provider.as_deref(), Some("fixture"));
            assert_eq!(report.actual_model.as_deref(), Some("served-model"));
            let NativeTokenCost::ConfiguredEstimate {
                micro_usd,
                normalized_usage,
                pricing_version,
                ..
            } = &report.token_cost
            else {
                anyhow::bail!("{outbound}: missing native estimate");
            };
            assert_eq!(*micro_usd, 32);
            assembled
                .app
                .language_model()
                .context("missing pipeline")?
                .drain_required_pending_settlements()
                .await?;
            let native_row = requests::Entity::find_by_id(&native.request_id)
                .one(&assembled.db)
                .await?
                .context("missing native settlement")?;
            let http_row = requests::Entity::find_by_id(&http_id)
                .one(&assembled.db)
                .await?
                .context("missing HTTP settlement")?;
            assert_eq!(
                native_row.charge_evidence_json,
                http_row.charge_evidence_json
            );
            assert_eq!(native_row.raw_usage_json, http_row.raw_usage_json);
            for row in [native_row, http_row] {
                assert_eq!(row.provider_id, "fixture");
                assert_eq!(row.model_id, "served-model");
                assert_eq!(row.charge_status, "computed");
                assert_eq!(row.usage_origin, "provider_reported");
                assert_eq!(row.estimated_charge_micro_usd, 32);
                assert_eq!(row.prompt_tokens, 10);
                assert_eq!(row.completion_tokens, 3);
                let evidence: ChargeEvidence = serde_json::from_str(
                    row.charge_evidence_json
                        .as_deref()
                        .context("missing charge evidence")?,
                )?;
                assert_eq!(&evidence.normalized_usage, normalized_usage);
                assert_eq!(&evidence.pricing_version, pricing_version);
            }
        }
        assert_eq!(requests::Entity::find().all(&assembled.db).await?.len(), 6);
    }
    assert_no_runtime_state_in_repository();
    Ok(())
}

fn assert_no_runtime_state_in_repository() {
    for file in [".installation.lock", "installation.id", "continuation.key"] {
        assert!(
            !std::path::Path::new(file).exists(),
            "fixture runtime state must stay in its temporary home: {file}"
        );
    }
}
