//! Actual private ingress, explicit resource-v3 publication and paired mixed sends.
//! Loopback peers implement the operator contract; this is not a provider-cloud test.
use super::*;
use axum::{body::Body as HttpBody, extract::Request, http::Version};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_ai::tenancy::{
    generate_content::{MIME, OrderedGenerateContentRequestV1},
    generation_defaults::GenerationDefaultsEnvelopeV2,
    private_adapter::{
        ATTEMPT_DOMAIN, ATTEMPT_HEADER, Attempt, Disposition, OUTCOME_DOMAIN, OUTCOME_HEADER,
        Outcome as AdapterOutcome,
    },
    resources_v3::ResourcePutV3,
    usage_evidence::{Protocol, Source},
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

mod boundaries;

struct NativePeer {
    ordinal: usize,
    reject: AtomicBool,
    bad_mac: AtomicBool,
    reply_data: Mutex<Option<(String, String)>>,
    held_stream: Mutex<Option<Arc<boundaries::StreamGate>>>,
    observed: Arc<Mutex<Vec<(String, HeaderMap)>>>,
    records: Mutex<Vec<(Attempt, Value, Version)>>,
}
fn usage() -> Value {
    json!({"promptTokenCount":2,"candidatesTokenCount":3,"totalTokenCount":7,"cachedContentTokenCount":0,"thoughtsTokenCount":2})
}
async fn receive(State(peer): State<Arc<NativePeer>>, request: Request) -> Response {
    match reply(&peer, request).await {
        Ok(response) => response,
        Err(error) => (
            StatusCode::INTERNAL_SERVER_ERROR,
            format!("fixture: {error:#}"),
        )
            .into_response(),
    }
}
async fn reply(peer: &NativePeer, request: Request) -> Result<Response> {
    let (parts, body) = request.into_parts();
    let bytes = axum::body::to_bytes(body, 16 * 1024 * 1024).await?;
    let body = crate::gateway::model::protocol::generate_content::native::value(
        std::str::from_utf8(&bytes)?,
    )?;
    let wire = parts
        .headers
        .get(ATTEMPT_HEADER)
        .context("claim header")?
        .to_str()?;
    let (payload, tag) = wire.split_once('.').context("claim envelope")?;
    let tag = URL_SAFE_NO_PAD.decode(tag)?;
    let payload = URL_SAFE_NO_PAD.decode(payload)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&private_adapter::KEY)?;
    mac.update(ATTEMPT_DOMAIN);
    mac.update(&payload);
    mac.verify_slice(&tag)
        .map_err(|_| anyhow::anyhow!("claim MAC"))?;
    let claim: Attempt = serde_json::from_slice(&payload)?;
    ensure!(claim.ordinal == u16::try_from(peer.ordinal)? && claim.method == "POST");
    ensure!(url::Url::parse(&claim.endpoint)?.path() == parts.uri.path());
    ensure!(claim.body_sha256 == URL_SAFE_NO_PAD.encode(Sha256::digest(&bytes)));
    ensure!(
        parts
            .headers
            .get("content-type")
            .is_some_and(|value| value == MIME)
    );
    ensure!(claim.stream == (body["stream"] == true));
    ensure!(body["model"] == format!("model-{}", peer.ordinal));
    ensure!(claim.credential.resource_id.as_str() == format!("static-slot-{}", peer.ordinal));
    ensure!(claim.profile == profiled::reference(peer.ordinal)?);
    peer.observed
        .lock()
        .map_err(|_| anyhow::anyhow!("observer"))?
        .push((format!("model-{}", peer.ordinal), parts.headers));
    peer.records
        .lock()
        .map_err(|_| anyhow::anyhow!("records"))?
        .push((claim.clone(), body.clone(), parts.version));
    let reject = peer.reject.load(Ordering::Acquire);
    let status = if reject { 429 } else { 200 };
    let (media, data) = if reject {
        (
            "application/json",
            json!({"error":{"code":429,"message":"fixture rejection"}}).to_string(),
        )
    } else if claim.stream {
        let first = json!({"responseId":"native-id","modelVersion":"fixture","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"receipt reply native","thoughtSignature":"signed-text"}]}}],"usageMetadata":{"promptTokenCount":2,"candidatesTokenCount":1,"totalTokenCount":3,"cachedContentTokenCount":0,"thoughtsTokenCount":0}});
        let terminal = json!({"candidates":[{"index":0,"finishReason":"STOP"},
            {"index":9,"content":{"parts":[{"functionCall":{"id":"raw-call","name":"native-tool","args":{"k":1}}}]}}]});
        let sibling = json!({"promptFeedback":{"safetyRatings":[]},"candidates":[{"index":9,"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}}]},"finishReason":"MAX_TOKENS"}]});
        let late = json!({"error":null,"usageMetadata":usage()});
        (
            "text/event-stream",
            format!("data: {first}\n\ndata: {terminal}\n\ndata: {sibling}\n\ndata: {late}\n\n"),
        )
    } else {
        ("application/json",json!({"responseId":"native-id","modelVersion":"fixture","candidates":[{"index":0,"content":{"role":"model","parts":[{"text":"receipt reply native","thoughtSignature":"signed-text"},{"inlineData":{"mimeType":"image/png","data":"AA=="}}]},"finishReason":"STOP"},
            {"index":9,"content":{"parts":[{"fileData":{"mimeType":"image/png","fileUri":"retained://native"}}]},"finishReason":"STOP"}],"usageMetadata":usage()}).to_string())
    };
    let replacement = peer
        .reply_data
        .lock()
        .map_err(|_| anyhow::anyhow!("reply data"))?
        .clone();
    let (media, data) = replacement.unwrap_or_else(|| (media.to_owned(), data));
    let retry_after = reject.then_some(3);
    let outcome = AdapterOutcome {
        disposition: if reject {
            Disposition::ProviderRejected
        } else if claim.stream {
            Disposition::Streaming
        } else {
            Disposition::Completed
        },
        status,
        retry_after,
    };
    let payload = serde_json::to_vec(&outcome)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&private_adapter::KEY)?;
    mac.update(OUTCOME_DOMAIN);
    mac.update(&tag);
    mac.update(&payload);
    let mut signature = mac.finalize().into_bytes().to_vec();
    if peer.bad_mac.load(Ordering::Acquire) {
        signature[0] ^= 1;
    }
    let gate = peer
        .held_stream
        .lock()
        .map_err(|_| anyhow::anyhow!("stream gate"))?
        .take();
    let body = if let Some(gate) = gate {
        gate.body(data)?
    } else {
        HttpBody::from(data)
    };
    let mut response = Response::builder()
        .status(status)
        .header("content-type", media)
        .header(
            OUTCOME_HEADER,
            format!(
                "{}.{}",
                URL_SAFE_NO_PAD.encode(payload),
                URL_SAFE_NO_PAD.encode(signature)
            ),
        )
        .body(body)?;
    if reject {
        response
            .headers_mut()
            .insert("retry-after", http::HeaderValue::from_static("3"));
    }
    Ok(response)
}
struct Harness {
    ordered: OrderedHarness,
    peer: Arc<NativePeer>,
    _peer: AbortOnDropHandle<std::io::Result<()>>,
}
impl Harness {
    async fn new(native_ordinal: usize, ordinary: &str, h2: bool) -> Result<Self> {
        Self::with_controls(native_ordinal, ordinary, h2, false).await
    }
    async fn with_controls(
        native_ordinal: usize,
        ordinary: &str,
        h2: bool,
        native_controls: bool,
    ) -> Result<Self> {
        let captured = Arc::new(Mutex::new(None));
        let capture = captured.clone();
        let mut profiles = Vec::new();
        for index in 0..2 {
            let mut profile = profiled::rules(1000, true)?;
            profile.rate.as_mut().context("rate")?.rate_group_id =
                format!("native-window-{index}").try_into()?;
            profiles.push(profile);
        }
        let mut ordered = OrderedHarness::with_native_mode(
            2,
            8,
            None,
            Value::Null,
            Some(&profiles),
            native_controls,
            move |rows| {
                *capture.lock().map_err(|_| anyhow::anyhow!("capture"))? = Some(
                    rows.iter()
                        .find(|row| row["reference"]["resource_kind"] == "route")
                        .context("route")?["definition"]
                        .clone(),
                );
                Ok(())
            },
        )
        .await?;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        let base = format!("http://{}/v1", listener.local_addr()?);
        let peer = Arc::new(NativePeer {
            ordinal: native_ordinal,
            reject: AtomicBool::new(native_ordinal == 1),
            bad_mac: AtomicBool::new(false),
            reply_data: Mutex::new(None),
            held_stream: Mutex::new(None),
            observed: ordered.observed.clone(),
            records: Mutex::new(Vec::new()),
        });
        let app = Router::new()
            .route("/v1/generate-content", post(receive))
            .with_state(peer.clone());
        let provider =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, app).await },
            ));
        let endpoint = url::Url::parse(&format!("{base}/generate-content"))?;
        let transport = if h2 {
            #[cfg(feature = "http2")]
            {
                crate::tenancy::execution::private_adapter::Peer::for_test_h2(
                    endpoint,
                    private_adapter::KEY,
                )?
            }
            #[cfg(not(feature = "http2"))]
            {
                anyhow::bail!("h2 test requires feature")
            }
        } else {
            crate::tenancy::execution::private_adapter::Peer::for_test(
                endpoint,
                private_adapter::KEY,
            )?
        };
        ordered.fixture.service.private_adapter = Some(Arc::new(transport));
        // Install the same real service with its exact paired peer before capability observation.
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "native-fixture".into(),
                    token_env: "NATIVE_FIXTURE_OPERATOR".into(),
                    scopes: vec![ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some(OPERATOR.into()),
        )?;
        let dispatch = Router::new()
            .fallback(|| async { StatusCode::NOT_FOUND })
            .layer(axum::middleware::from_fn_with_state(
                ordered.fixture.service.clone(),
                crate::tenancy::http::dispatch,
            ));
        let app =
            crate::tenancy::http::router(ordered.fixture.service.clone(), auth).merge(dispatch);
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
        ordered.endpoint = format!("http://{}", listener.local_addr()?);
        ordered._gateway =
            AbortOnDropHandle::new(tokio::spawn(
                async move { axum::serve(listener, app).await },
            ));
        let control = ordered.control(true, HttpVersion::Http1Only)?;
        ensure!(
            control
                .resource_capabilities_v3(&ordered.fixture.installation)
                .await
                .context("native resource capability")?
                .provisioning_enabled
        );
        // A preexisting Chat permission/root must never expose a native root.
        ensure!(
            control
                .candidate_manifest(
                    &ordered.fixture.installation,
                    &ordered.fixture.tenant,
                    &ordered.revision.revision_id,
                    &ordered.route,
                    GenerationProtocol::GenerateContent
                )
                .await
                .is_err()
        );
        let definition = captured
            .lock()
            .map_err(|_| anyhow::anyhow!("capture"))?
            .take()
            .context("captured route")?;
        let mut definition = definition;
        definition["kind"] = "native_profiled_route_v1".into();
        for index in 0..2 {
            let name = format!("provider-{index}");
            definition["routing"]["providers"][&name]["api_protocol"] = if index == native_ordinal {
                json!([{"*":["ordered_generate_content","chat_completions","messages","responses"]}])
            } else {
                json!([{"*":ordinary}])
            };
            if index == native_ordinal {
                definition["routing"]["providers"][&name]["api_base"] = json!(base);
            }
        }
        let extra = definition["attachments"]
            .as_array()
            .context("attachments")?
            .iter()
            .filter(|item| item["inbound_protocol"] == "chat_completions")
            .cloned()
            .map(|mut item| {
                item["inbound_protocol"] = "generate_content".into();
                item
            })
            .collect::<Vec<_>>();
        definition["attachments"]
            .as_array_mut()
            .context("attachments")?
            .extend(extra);
        let status = control
            .resource_status(
                &ordered.fixture.installation,
                &ordered.fixture.tenant,
                &ordered.route,
            )
            .await
            .context("native resource status")?;
        let command: ResourcePutV3 = serde_json::from_value(
            json!({"format":3,"operation_id":"native-publish","installation_id":ordered.fixture.installation,"key":ordered.route.key(),"expected_revision":status.head_revision,"definition":definition}),
        )?;
        let published = control
            .put_resource_v3(&ordered.fixture.tenant, &command)
            .await
            .map_err(|error| match &error {
                bitrouter_sdk::error::Error::Tenancy(failure) => anyhow::anyhow!(
                    "native resource publication: {error}; body={:?}",
                    failure.body
                ),
                _ => anyhow::anyhow!("native resource publication: {error:?}"),
            })?;
        ordered.route.revision_id = published
            .resource_revision
            .context("published native revision")?;
        let mut configuration = ordered
            .fixture
            .configuration_input("native-current-draft")?;
        configuration.document.routes.push(ordered.route.clone());
        configuration.document.generation_defaults.max_output_tokens = 25;
        configuration.document.limits.active_executions = 32;
        configuration.document.limits.queued_work = 32;
        ordered.revision = match ordered
            .fixture
            .service
            .execute(
                ordered.fixture.operator.clone(),
                crate::tenancy::command::Command::Configuration(
                    ordered.fixture.tenant.clone(),
                    configuration,
                ),
            )
            .await
            .map_err(failure)?
        {
            crate::tenancy::store::StoredResponse::Configuration(revision) => revision,
            _ => anyhow::bail!("configuration result"),
        };
        Ok(Self {
            ordered,
            peer,
            _peer: provider,
        })
    }
    async fn invoke(&self, stream: bool, version: HttpVersion) -> Result<ExecutionId> {
        let binding = self
            .ordered
            .binding(vec![1, 0], GenerationProtocol::GenerateContent)
            .await?;
        let defaults: GenerationDefaultsEnvelopeV2 = serde_json::from_value(
            json!({"schema_version":2,"canonical":{"schema_version":1,"max_tokens":17},"supplemental":{},"configuration_defaults":"preserve_absence"}),
        )?;
        let binding = binding.with_generation_defaults_envelope_v2(&defaults)?;
        let control = self.ordered.control(true, version)?;
        let capabilities = control
            .ordered_generate_content_capabilities()
            .await
            .context("native ingress capability discovery")?;
        let gateway = GatewayClient::with_options(
            &self.ordered.endpoint,
            None,
            TransportOptions {
                http_version: version,
                ..Default::default()
            },
        )?;
        let request = OrderedGenerateContentRequestV1 {
            schema_version: 1,
            model: MODEL.into(),
            stream,
            request: [("contents".into(), json!([{"parts":[{"text":"hello"}]}]))].into(),
        };
        let reply = gateway
            .ordered_tenant(self.ordered.token()?, &binding)?
            .with_generate_content(&capabilities)?
            .with_generation_defaults_envelope_v2(defaults)?
            .generate_content_reply(&request)
            .await?;
        let id = reply.metadata.execution_id.context("execution id")?;
        match reply.body {
            Body::Json(body) => {
                ensure!(!stream && body["candidates"].is_array());
                if self.peer.ordinal == 0 {
                    ensure!(
                        body["candidates"]
                            .as_array()
                            .context("raw candidates")?
                            .len()
                            == 2
                    );
                    ensure!(body["candidates"][1]["index"] == 9);
                    ensure!(
                        body["candidates"][0]["content"]["parts"][0]["thoughtSignature"]
                            == "signed-text"
                    );
                }
            }
            Body::Stream(mut events) => {
                ensure!(stream);
                let mut ended = false;
                let mut raw = Vec::new();
                while let Some(item) = events.next().await {
                    match item? {
                        StreamItem::End(end) => {
                            ensure!(end == ProtocolEnd::GenerateContentFinished && !ended);
                            ended = true
                        }
                        StreamItem::Event(event) => {
                            ensure!(!ended);
                            raw.push(event.data)
                        }
                    }
                }
                ensure!(ended);
                if self.peer.ordinal == 0 {
                    let raw = serde_json::to_string(&raw)?;
                    ensure!(
                        raw.contains("signed-text")
                            && raw.contains("native-tool")
                            && raw.contains("inlineData")
                            && raw.contains("MAX_TOKENS")
                    );
                }
            }
        }
        Ok(id)
    }
}
#[tokio::test]
async fn native_generate_content_actual_mixed_json_sse_original_order_and_counters() -> Result<()> {
    for ordinary in ["chat_completions", "messages", "responses"] {
        for native_ordinal in [0, 1] {
            for stream in [false, true] {
                for h2 in [false, true] {
                    if h2 && !cfg!(feature = "http2") {
                        continue;
                    }
                    let harness = Harness::new(native_ordinal, ordinary, h2)
                        .await
                        .with_context(|| {
                            format!("setup native={native_ordinal},ordinary={ordinary},h2={h2}")
                        })?;
                    let version = if h2 {
                        #[cfg(feature = "http2")]
                        {
                            HttpVersion::Http2PriorKnowledge
                        }
                        #[cfg(not(feature = "http2"))]
                        {
                            anyhow::bail!("h2 fixture requires feature")
                        }
                    } else {
                        HttpVersion::Http1Only
                    };
                    let id = harness.invoke(stream, version).await.with_context(|| {
                        format!(
                            "native={native_ordinal},ordinary={ordinary},stream={stream},h2={h2}"
                        )
                    })?;
                    let receipt = harness.ordered.receipt(&id).await?;
                    ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 2);
                    ensure!(
                        receipt.accepted_selection.inbound_protocol
                            == GenerationProtocol::GenerateContent
                            && receipt.accepted_selection.candidate_indices == [1, 0]
                    );
                    ensure!(harness.ordered.observed()? == ["model-1", "model-0"]);
                    {
                        let records = harness
                            .peer
                            .records
                            .lock()
                            .map_err(|_| anyhow::anyhow!("records"))?;
                        ensure!(records.len() == 1);
                        let (claim, body, wire) = &records[0];
                        ensure!(
                            claim.execution_id == id
                                && claim.rank == u16::from(native_ordinal == 0)
                        );
                        ensure!(
                            *wire
                                == if h2 {
                                    Version::HTTP_2
                                } else {
                                    Version::HTTP_11
                                }
                        );
                        ensure!(
                            body["request"] == json!({"contents":[{"parts":[{"text":"hello"}]}]})
                        );
                        ensure!(
                            body["bitrouter_generation_defaults_envelope"]["canonical"]["max_tokens"]
                                == 17
                        );
                    }
                    let control = harness.ordered.control(false, version)?;
                    let evidence = control.usage_evidence_v2(&receipt).await?;
                    ensure!(evidence.extractor_version == 2);
                    if native_ordinal == 0 {
                        let frames = evidence
                            .frames
                            .iter()
                            .filter(|frame| frame.protocol == Protocol::GenerateContent)
                            .collect::<Vec<_>>();
                        ensure!(!frames.is_empty());
                        ensure!(frames.iter().all(|frame| frame.source
                            == if stream {
                                Source::GenerateContentChunk
                            } else {
                                Source::Json
                            }));
                        ensure!(
                            receipt.attempts[1].usage.output_tokens == Some(5)
                                && receipt.attempts[1].usage.cache_write_tokens.is_none()
                                && !receipt.attempts[1].usage.complete
                        );
                    }
                    let manifest = harness
                        .ordered
                        .control(true, version)?
                        .candidate_manifest(
                            &harness.ordered.fixture.installation,
                            &harness.ordered.fixture.tenant,
                            &harness.ordered.revision.revision_id,
                            &harness.ordered.route,
                            GenerationProtocol::GenerateContent,
                        )
                        .await?;
                    for observed in manifest
                        .runtime_observation
                        .context("actual-hop observation")?
                        .candidates
                    {
                        ensure!(observed.rate.context("actual-hop rate")?.shared_count == 1);
                    }
                    harness.ordered.fixture.service.shutdown().await?;
                }
            }
        }
    }
    Ok(())
}
