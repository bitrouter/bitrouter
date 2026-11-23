//! Actual retained Google Chat sends keep credential-bound JSON/SSE continuity.

use super::*;
use crate::gateway::{
    model::{
        protocol::{OutboundAdapter, chat_completions::ChatCompletionsAdapter},
        target::ModelTarget,
    },
    providers::google_chat,
};
use axum::response::IntoResponse;

fn signed_completion() -> Value {
    json!({
        "id":"chatcmpl-google", "object":"chat.completion", "model":"native-shared",
        "choices":[{"index":0,"message":{"role":"assistant","content":null,"tool_calls":[{
            "id":"call-1","type":"function","function":{"name":"lookup","arguments":"{\"q\":\"ok\"}"},
            "extra_content":{"google":{"thought_signature":"signed"}}
        }]},"finish_reason":"tool_calls"}],
        "usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}
    })
}

struct GoogleProvider {
    url: String,
    received: tokio::sync::mpsc::Receiver<(HeaderMap, Value)>,
    calls: Arc<AtomicUsize>,
    _task: AbortOnDropHandle<std::io::Result<()>>,
}

type ProviderCapture = (
    tokio::sync::mpsc::Sender<(HeaderMap, Value)>,
    Arc<AtomicUsize>,
);

impl GoogleProvider {
    async fn start() -> Result<Self> {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let url = format!("http://localhost:{}/v1", listener.local_addr()?.port());
        let (sent, received) = tokio::sync::mpsc::channel(16);
        let calls = Arc::new(AtomicUsize::new(0));
        let router = Router::new().route("/v1/chat/completions", post(
            |State((sent, calls)): State<ProviderCapture>,
             headers: HeaderMap, Json(body): Json<Value>| async move {
                calls.fetch_add(1, Ordering::SeqCst);
                let stream = body["stream"] == true;
                let replay = body["messages"].as_array().is_some_and(|messages| messages.iter().any(|m| m["role"] == "assistant"));
                if sent.try_send((headers, body)).is_err() {
                    return StatusCode::SERVICE_UNAVAILABLE.into_response();
                }
                if stream {
                    let chunks = [
                        json!({"id":"chatcmpl-google","model":"native-shared","choices":[{"index":0,"delta":{"role":"assistant","tool_calls":[{"index":0,"id":"call-1","type":"function","function":{"name":"lookup","arguments":"{\"q\":"},"extra_content":{"google":{"thought_signature":"signed"}}}]},"finish_reason":null}]}),
                        json!({"id":"chatcmpl-google","model":"native-shared","choices":[{"index":0,"delta":{"tool_calls":[{"index":0,"function":{"arguments":"\"ok\"}"}}]},"finish_reason":null}]}),
                        json!({"id":"chatcmpl-google","model":"native-shared","choices":[{"index":0,"delta":{},"finish_reason":"tool_calls"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}}),
                    ];
                    let mut wire = chunks.into_iter().map(|chunk| format!("data: {chunk}\n\n")).collect::<String>();
                    wire.push_str("data: [DONE]\n\n");
                    return ([("content-type", "text/event-stream")], wire).into_response();
                }
                let response = if replay {
                    json!({"id":"chatcmpl-replay","object":"chat.completion","model":"native-shared","choices":[{"index":0,"message":{"role":"assistant","content":"replay accepted"},"finish_reason":"stop"}],"usage":{"prompt_tokens":2,"completion_tokens":1,"total_tokens":3}})
                } else { signed_completion() };
                Json(response).into_response()
            }
        )).with_state((sent, calls.clone()));
        let task =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, router).await },
            ));
        Ok(Self {
            url,
            received,
            calls,
            _task: task,
        })
    }

    async fn observed(&mut self, secret: &str, replay: bool) -> Result<()> {
        let (headers, body) = tokio::time::timeout(TIMEOUT, self.received.recv())
            .await?
            .context("Google provider not called")?;
        ensure!(
            headers
                .get("authorization")
                .context("Bearer missing")?
                .to_str()?
                == format!("Bearer {secret}")
        );
        ensure!(!headers.contains_key("x-goog-api-key") && !headers.contains_key("x-api-key"));
        ensure!(body["model"] == "native-shared");
        let wire = body.to_string();
        ensure!(!wire.contains("google_replay_proof") && !wire.contains(secret));
        if replay {
            ensure!(body["messages"].as_array().context("messages")?.iter().any(
                |m| m["tool_calls"][0]["extra_content"]["google"]["thought_signature"] == "signed"
            ));
        }
        Ok(())
    }
}

async fn fixture(url: &str) -> Result<Harness> {
    let mut harness = Harness::new(url).await?;
    harness.config = crate::config::parse(&catalog_fixture_yaml(url, SECRET).replace(
        "provider_model_id: native-shared}",
        "provider_model_id: native-shared, compatibility: {chat_completions: {google_extensions: true}}}",
    ))?;
    harness.restart().await
}

async fn send(
    fixture: &Fixture,
    credential: &CredentialIssued,
    descriptor: &CatalogCandidatesV1,
    mut body: Value,
    selection: Option<&CatalogSelectionRequestV1>,
) -> Result<(StatusCode, String)> {
    body["model"] = ROUTE.into();
    if let Some(selection) = selection {
        body[bitrouter_ai::tenancy::catalog::BODY_MEMBER] = serde_json::to_value(selection)?;
    }
    let router = Router::new()
        .fallback(|| async { StatusCode::NOT_FOUND })
        .layer(axum::middleware::from_fn_with_state(
            fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let mut request =
        Request::post("/v1/chat/completions").body(Body::from(serde_json::to_vec(&body)?))?;
    *request.headers_mut() = headers(fixture, credential, descriptor).await?;
    request
        .headers_mut()
        .insert("content-type", "application/json".parse()?);
    let response = tokio::time::timeout(TIMEOUT, router.oneshot(request)).await??;
    let status = response.status();
    let bytes = tokio::time::timeout(
        TIMEOUT,
        axum::body::to_bytes(response.into_body(), 64 * 1024),
    )
    .await??;
    tokio::time::timeout(TIMEOUT, fixture.service.admission.wait_drained()).await?;
    Ok((status, String::from_utf8(bytes.to_vec())?))
}

fn replay(call: Value) -> Value {
    json!({"messages":[
        {"role":"user","content":"lookup"},
        {"role":"assistant","content":null,"tool_calls":[call]},
        {"role":"tool","tool_call_id":"call-1","content":"found"}
    ]})
}

fn stream_call(wire: &str) -> Result<Value> {
    let mut call = json!({"id":"","type":"function","function":{"name":"","arguments":""}});
    let mut terminal = false;
    let mut proof = false;
    ensure!(
        wire.contains("data: [DONE]"),
        "missing SSE terminal: {wire}"
    );
    for data in wire.lines().filter_map(|line| line.strip_prefix("data: ")) {
        if data == "[DONE]" {
            continue;
        }
        let chunk: Value = serde_json::from_str(data)?;
        ensure!(chunk.get("error").is_none(), "stream error: {chunk}");
        if chunk["choices"][0]["finish_reason"].as_str().is_some() {
            ensure!(proof, "Google proof must precede the terminal");
            terminal = true;
        }
        if let Some(calls) = chunk["choices"][0]["delta"]["tool_calls"].as_array() {
            for delta in calls {
                if let Some(id) = delta["id"].as_str() {
                    call["id"] = id.into();
                }
                for key in ["name", "arguments"] {
                    if let Some(value) = delta["function"][key].as_str() {
                        let saved = call["function"][key].as_str().context("tool field")?;
                        call["function"][key] = format!("{saved}{value}").into();
                    }
                }
                if let Some(extra) = delta.get("extra_content") {
                    call["extra_content"] = extra.clone();
                    proof = extra["bitrouter"]["google_replay_proof"].as_str().is_some();
                }
            }
        }
    }
    ensure!(terminal && proof);
    ensure!(
        call["id"] == "call-1"
            && call["function"]["name"] == "lookup"
            && call["function"]["arguments"] == "{\"q\":\"ok\"}"
    );
    Ok(call)
}

async fn roundtrip(stream: bool, owned: bool) -> Result<()> {
    let mut provider = GoogleProvider::start().await?;
    let mut harness = fixture(&provider.url).await?;
    let initial = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let (descriptor, selection) = if owned {
        let slot = owned_slot(&harness.fixture, "google-owned").await?;
        let selection = CatalogSelectionRequestV1 {
            schema_version: 1,
            catalog_revision: initial.manifest.execution_revision.clone(),
            route_id: ROUTE.try_into()?,
            inbound_protocol: GenerationProtocol::ChatCompletions,
            candidates: vec![CatalogCandidateV1 {
                template_id: initial.templates[0].template_id.clone(),
                credential_ref: Some(slot),
                route_ref: None,
            }],
        };
        let prepared = harness
            .fixture
            .service
            .prepare_catalog_selection(
                &harness.fixture.operator,
                &harness.fixture.tenant,
                &selection,
            )
            .await
            .map_err(failure)?;
        (prepared.candidates, Some(selection))
    } else {
        (initial, None)
    };
    let credential = grant(
        &mut harness.fixture,
        scope(&descriptor),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "google-grant",
    )
    .await?;
    let before = publication_counts(&harness.fixture).await?;
    let (status, wire) = send(
        &harness.fixture,
        &credential,
        &descriptor,
        json!({"stream":stream,"messages":[{"role":"user","content":"lookup"}]}),
        selection.as_ref(),
    )
    .await?;
    ensure!(
        status == StatusCode::OK,
        "Google send failed: {status} {wire}"
    );
    let call = if stream {
        stream_call(&wire)?
    } else {
        let response: Value = serde_json::from_str(&wire)?;
        response["choices"][0]["message"]["tool_calls"][0].clone()
    };
    ensure!(
        call["extra_content"]["bitrouter"]["google_replay_proof"]
            .as_str()
            .is_some()
    );
    let secret = if owned { OWNED_SECRET } else { SECRET };
    provider.observed(secret, false).await?;
    let (status, wire) = send(
        &harness.fixture,
        &credential,
        &descriptor,
        replay(call),
        selection.as_ref(),
    )
    .await?;
    ensure!(
        status == StatusCode::OK,
        "Google replay failed: {status} {wire}"
    );
    let response: Value = serde_json::from_str(&wire)?;
    ensure!(response["choices"][0]["message"]["content"] == "replay accepted");
    provider.observed(secret, true).await?;
    ensure!(provider.calls.load(Ordering::SeqCst) == 2);
    ensure!(publication_counts(&harness.fixture).await? == before);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn configured_google_chat_json_binds_and_replays_actual_platform_credential() -> Result<()> {
    roundtrip(false, false).await
}

#[tokio::test]
async fn configured_google_chat_sse_binds_and_replays_actual_owned_credential() -> Result<()> {
    roundtrip(true, true).await
}

#[tokio::test]
async fn configured_google_chat_rejects_other_endpoint_model_key_and_account_before_send()
-> Result<()> {
    let provider = GoogleProvider::start().await?;
    let mut harness = fixture(&provider.url).await?;
    let descriptor = catalog(
        &harness.fixture,
        &harness.fixture.operator,
        &harness.fixture.tenant,
    )
    .await?;
    let credential = grant(
        &mut harness.fixture,
        scope(&descriptor),
        TenantRole::TenantAdmin.permissions().to_vec(),
        "google-rejections",
    )
    .await?;
    let model = harness.config.resolve_router("acme/shared")?;
    let targets = crate::config::routing_table::resolve_clean_route_chain(
        &harness.config,
        &model.clean_model,
        &model.prefs,
    )?;
    let selected = targets.first().context("Google target")?.model_target();
    ensure!(selected.compatibility.chat_completions.google_extensions);
    for change in ["endpoint", "model", "key", "account"] {
        let mut other: ModelTarget = selected.clone();
        match change {
            "endpoint" => other.api_base.push_str("/other"),
            "model" => other.service_id.push_str("-other"),
            "key" => other.api_key.push_str("-other"),
            "account" => other.account_label = Some("other".into()),
            _ => anyhow::bail!("unknown mismatch"),
        }
        let result = google_chat::bind_result(
            ChatCompletionsAdapter.parse_response(signed_completion())?,
            &other,
        )?;
        let content = result.content.first().context("signed call")?;
        let bitrouter_ai::types::Content::ToolCall {
            provider_metadata, ..
        } = content
        else {
            anyhow::bail!("tool call missing")
        };
        let proof = bitrouter_ai::types::provider_namespace(provider_metadata, "google")
            .context("Google metadata")?
            .get("replayProof")
            .context("Google proof")?;
        let mut call = signed_completion()["choices"][0]["message"]["tool_calls"][0].clone();
        call["extra_content"]["bitrouter"] = json!({"google_replay_proof":proof});
        let (status, _) = send(
            &harness.fixture,
            &credential,
            &descriptor,
            replay(call),
            None,
        )
        .await?;
        ensure!(
            status.is_client_error(),
            "accepted wrong {change}: {status}"
        );
        ensure!(
            provider.calls.load(Ordering::SeqCst) == 0,
            "wrong {change} reached provider"
        );
    }
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[test]
fn google_chat_authenticated_wire_still_rejects_endpoint_model_and_bearer_changes() -> Result<()> {
    let config = crate::config::parse(&catalog_fixture_yaml("https://selected.example/v1", SECRET).replace(
        "provider_model_id: native-shared}",
        "provider_model_id: native-shared, compatibility: {chat_completions: {google_extensions: true}}}",
    ))?;
    let resolution = config.resolve_router("acme/shared")?;
    let targets = crate::config::routing_table::resolve_clean_route_chain(
        &config,
        &resolution.clean_model,
        &resolution.prefs,
    )?;
    let target = targets
        .first()
        .context("selected Google target")?
        .model_target();
    let build = |endpoint: &str, model: &str, key: &str| -> Result<reqwest::Request> {
        Ok(reqwest::Client::new()
            .post(endpoint)
            .bearer_auth(key)
            .json(&json!({"model":model}))
            .build()?)
    };
    let endpoint = format!("{}/chat/completions", target.api_base);
    google_chat::validate_authenticated_request(
        &build(&endpoint, &target.service_id, SECRET)?,
        &target,
    )?;
    for request in [
        build(
            "https://other.example/v1/chat/completions",
            &target.service_id,
            SECRET,
        )?,
        build(&endpoint, "other-model", SECRET)?,
        build(&endpoint, &target.service_id, "other-key")?,
    ] {
        ensure!(google_chat::validate_authenticated_request(&request, &target).is_err());
    }
    let mut duplicate = build(&endpoint, &target.service_id, SECRET)?;
    duplicate
        .headers_mut()
        .append("authorization", format!("Bearer {SECRET}").parse()?);
    ensure!(google_chat::validate_authenticated_request(&duplicate, &target).is_err());
    Ok(())
}
