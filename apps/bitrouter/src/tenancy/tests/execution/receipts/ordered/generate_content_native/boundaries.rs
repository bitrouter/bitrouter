//! The actual mounted token boundary and non-replay rules, without a second sender.
use super::*;
use bitrouter_ai::tenancy::generation_defaults::ENVELOPE_BODY_MEMBER;

mod process_consumer;

pub(super) struct StreamGate {
    entered: tokio::sync::Notify,
    dropped: tokio::sync::Notify,
    closed: AtomicBool,
    release: tokio::sync::Semaphore,
}
struct StreamReadOwner(Arc<StreamGate>);
impl Drop for StreamReadOwner {
    fn drop(&mut self) {
        self.0.closed.store(true, Ordering::Release);
        self.0.dropped.notify_one();
    }
}
impl StreamGate {
    fn new() -> Arc<Self> {
        Arc::new(Self {
            entered: tokio::sync::Notify::new(),
            dropped: tokio::sync::Notify::new(),
            closed: AtomicBool::new(false),
            release: tokio::sync::Semaphore::new(0),
        })
    }
    pub(super) fn body(self: Arc<Self>, data: String) -> Result<HttpBody> {
        let (first, tail) = data.split_once("\n\n").context("held SSE fixture")?;
        let first = format!("{first}\n\n");
        let tail = tail.to_owned();
        let owner = StreamReadOwner(self.clone());
        let stream = futures::stream::once(async move { Ok::<_, std::io::Error>(first) }).chain(
            futures::stream::once(async move {
                let _owner = owner;
                self.entered.notify_one();
                self.release
                    .acquire()
                    .await
                    .map_err(std::io::Error::other)?
                    .forget();
                Ok(tail)
            }),
        );
        Ok(HttpBody::from_stream(stream))
    }
}

impl Harness {
    async fn raw(
        &self,
        indices: Vec<u16>,
        raw: &str,
        defaults: Option<&GenerationDefaultsEnvelopeV2>,
        opt_in: bool,
        media: &str,
    ) -> Result<reqwest::Response> {
        let binding = self
            .ordered
            .binding(indices, GenerationProtocol::GenerateContent)
            .await?;
        let binding = match defaults {
            Some(defaults) => binding.with_generation_defaults_envelope_v2(defaults)?,
            None => binding,
        };
        let mut headers = caller_timeout::ordered_headers(&self.ordered, &binding)?;
        if opt_in {
            headers.insert(
                bitrouter_ai::tenancy::native_controls::HEADER,
                bitrouter_ai::tenancy::native_controls::PROVIDER_NATIVE_V1.parse()?,
            );
        }
        let body = if let Some(defaults) = defaults {
            ensure!(raw.starts_with('{'));
            format!(
                "{{\"{ENVELOPE_BODY_MEMBER}\":{},{}",
                serde_json::to_string(defaults)?,
                &raw[1..]
            )
        } else {
            raw.to_owned()
        };
        Ok(reqwest::Client::builder()
            .no_proxy()
            .build()?
            .post(format!(
                "{}/{}",
                self.ordered.endpoint,
                bitrouter_ai::tenancy::generate_content::PATH
            ))
            .headers(headers)
            .header("content-type", media)
            .body(body)
            .send()
            .await?)
    }

    async fn no_native_work(&self) -> Result<()> {
        ensure!(self.ordered.observed()?.is_empty());
        ensure!(
            self.peer
                .records
                .lock()
                .map_err(|_| anyhow::anyhow!("records"))?
                .is_empty()
        );
        let control = self.ordered.control(true, HttpVersion::Http1Only)?;
        let page = control
            .executions(
                &self.ordered.fixture.installation,
                &self.ordered.fixture.tenant,
                &"ordered-correlation".try_into()?,
                None,
                100,
            )
            .await?;
        ensure!(page.execution_ids.is_empty());
        let manifest = control
            .candidate_manifest(
                &self.ordered.fixture.installation,
                &self.ordered.fixture.tenant,
                &self.ordered.revision.revision_id,
                &self.ordered.route,
                GenerationProtocol::GenerateContent,
            )
            .await?;
        for candidate in manifest
            .runtime_observation
            .context("native runtime observation")?
            .candidates
        {
            ensure!(candidate.rate.context("rate")?.shared_count == 0);
        }
        Ok(())
    }
}

fn envelope(request: &str, stream: bool) -> String {
    format!(
        "{{\"schema_version\":1,\"model\":\"{MODEL}\",\"stream\":{stream},\"request\":{request}}}"
    )
}

fn defaults(supplemental: Value) -> Result<GenerationDefaultsEnvelopeV2> {
    Ok(serde_json::from_value(
        json!({"schema_version":2,"canonical":{"schema_version":1,"max_tokens":17},
        "supplemental":supplemental,"configuration_defaults":"preserve_absence"}),
    )?)
}

#[tokio::test]
async fn native_generate_content_mounted_discovery_without_peer_is_disabled_and_ordinary_still_works()
-> Result<()> {
    let ordered = OrderedHarness::with_profiles(
        1,
        8,
        None,
        Value::Null,
        Some(&[profiled::rules(1000, false)?]),
    )
    .await?;
    let control = ordered.control(true, HttpVersion::Http1Only)?;
    let capabilities = control.ordered_generate_content_capabilities().await?;
    ensure!(!capabilities.enabled && !capabilities.cached_content_enabled);
    ensure!(
        !control
            .resource_capabilities_v3(&ordered.fixture.installation)
            .await?
            .provisioning_enabled
    );
    let binding = ordered
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    let raw = envelope(r#"{"contents":[{"parts":[{"text":"hello"}]}]}"#, false);
    let response = reqwest::Client::builder()
        .no_proxy()
        .build()?
        .post(format!(
            "{}/{}",
            ordered.endpoint,
            bitrouter_ai::tenancy::generate_content::PATH
        ))
        .headers(caller_timeout::ordered_headers(&ordered, &binding)?)
        .header("content-type", MIME)
        .body(raw)
        .send()
        .await?;
    ensure!(
        response.status() == StatusCode::CONFLICT
            && !response.headers().contains_key("x-bitrouter-request-id")
    );
    ensure!(ordered.observed()?.is_empty());
    let id = ordered
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    ensure!(
        ordered.receipt(&id).await?.outcome == Outcome::Completed
            && ordered.observed()? == ["model-0"]
    );
    ordered.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_generate_content_mounted_original_tokens_survive_opaque_number_keys_and_rich_parts()
-> Result<()> {
    let harness = Harness::new(0, "chat_completions", false).await?;
    let original = r#"{"contents":[{"parts":[{"text":"signed","thoughtSignature":"proof"},{"unknownPart":{"must":"remain"}},{"functionCall":{"id":"native-call","name":"tool","args":{"$serde_json::private::Number":"literal","sibling":[1.25,{"$serde_json::private::RawValue":"opaque"}]}}}]}],"opaque":{"escaped\u004bey":2.5e1}}"#;
    let response = harness
        .raw(vec![1, 0], &envelope(original, false), None, false, MIME)
        .await?;
    let status = response.status();
    let id: ExecutionId = response
        .headers()
        .get("x-bitrouter-request-id")
        .context("execution header")?
        .to_str()?
        .to_owned()
        .try_into()?;
    let body = response.text().await?;
    ensure!(status == StatusCode::OK, "{status}: {body}");
    let body = crate::gateway::model::protocol::generate_content::native::value(&body)?;
    ensure!(body["candidates"][0]["content"]["parts"][0]["thoughtSignature"] == "signed-text");
    ensure!(body["candidates"][0]["content"]["parts"][1]["inlineData"]["mimeType"] == "image/png");
    ensure!(harness.ordered.observed()? == ["model-0"]);
    let receipt = harness.ordered.receipt(&id).await?;
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].candidate_index == 0);
    {
        let records = harness
            .peer
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("records"))?;
        ensure!(records.len() == 1 && records[0].0.rank == 1);
        ensure!(
            records[0].1["request"]
                == crate::gateway::model::protocol::generate_content::native::value(original)?
        );
        ensure!(records[0].1.get(ENVELOPE_BODY_MEMBER).is_none());
    }
    harness.ordered.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_generate_content_mounted_duplicate_depth_and_competing_members_refuse_before_arm()
-> Result<()> {
    let harness = Harness::new(0, "chat_completions", false).await?;
    let ordinary = r#"{"contents":[{"parts":[{"text":"hello"}]}]}"#;
    let good = envelope(ordinary, false);
    let invalid = [
        good.replacen("\"model\":", "\"mo\\u0064el\":\"other\",\"model\":", 1),
        envelope(r#"{"opaque":{"k":1,"\u006b":2}}"#, false),
        envelope(r#"{"model":"competing"}"#, false),
        envelope(r#"{"stream":true}"#, false),
        envelope(
            &format!("{{\"opaque\":{}0{}}}", "[".repeat(128), "]".repeat(128)),
            false,
        ),
        good.replace(
            "\"schema_version\":1",
            "\"schema_version\":1.00000000000000001",
        ),
    ];
    for raw in invalid {
        let response = harness.raw(vec![0], &raw, None, false, MIME).await?;
        ensure!(
            response.status() == StatusCode::BAD_REQUEST,
            "{}",
            response.text().await?
        );
        ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
    }
    let response = harness
        .raw(vec![0], &good, None, false, "application/json")
        .await?;
    ensure!(!response.status().is_success());
    ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
    harness.no_native_work().await?;
    harness.ordered.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_generate_content_mounted_cache_policy_and_source_null_preserve_defaults_commitment()
-> Result<()> {
    let harness = Harness::new(0, "chat_completions", false).await?;
    let cache =
        defaults(json!({"__google_top_level__":{"cachedContent":"cachedContents/native"}}))?;
    let plain = envelope(r#"{"contents":[{"parts":[{"text":"hello"}]}]}"#, false);
    for (raw, layer, opt_in) in [
        (
            envelope(r#"{"cachedContent":"cachedContents/native"}"#, false),
            None,
            false,
        ),
        (
            envelope(r#"{"cachedContent":"cachedContents/native"}"#, false),
            None,
            true,
        ),
        (plain.clone(), Some(&cache), false),
        (
            envelope(r#"{"cached_content":"cachedContents/native"}"#, false),
            None,
            false,
        ),
    ] {
        let response = harness.raw(vec![0], &raw, layer, opt_in, MIME).await?;
        ensure!(response.status() == StatusCode::CONFLICT);
        ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
    }
    harness.no_native_work().await?;
    let raw = envelope(
        r#"{"contents":[{"parts":[{"text":"hello"}]}],"cachedContent":null}"#,
        false,
    );
    let response = harness
        .raw(vec![0], &raw, Some(&cache), false, MIME)
        .await?;
    let status = response.status();
    let body = response.text().await?;
    ensure!(status == StatusCode::OK, "{status}: {body}");
    {
        let records = harness
            .peer
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("records"))?;
        ensure!(records.len() == 1 && records[0].1["request"]["cachedContent"].is_null());
        ensure!(
            records[0].1[ENVELOPE_BODY_MEMBER]["supplemental"]["__google_top_level__"]["cachedContent"]
                == "cachedContents/native"
        );
    }
    harness.ordered.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_generate_content_mounted_unknown_bridge_ack_never_advances_or_replays() -> Result<()>
{
    for stream in [false, true] {
        let harness = Harness::new(1, "responses", false).await?;
        harness.peer.bad_mac.store(true, Ordering::Release);
        ensure!(
            harness
                .invoke(stream, HttpVersion::Http1Only)
                .await
                .is_err()
        );
        ensure!(harness.ordered.observed()? == ["model-1"]);
        let id = {
            let records = harness
                .peer
                .records
                .lock()
                .map_err(|_| anyhow::anyhow!("records"))?;
            ensure!(records.len() == 1);
            records[0].0.execution_id.clone()
        };
        let receipt = harness.ordered.receipt(&id).await?;
        ensure!(receipt.state == ReceiptState::Unknown && receipt.attempts.len() == 1);
        harness.ordered.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn native_generate_content_mounted_cache_opt_in_excludes_foreign_before_actual_hop()
-> Result<()> {
    let harness = Harness::with_controls(0, "responses", false, true).await?;
    let raw = envelope(
        r#"{"contents":[{"parts":[{"text":"hello"}]}],"cachedContent":"cachedContents/native"}"#,
        false,
    );
    let response = harness.raw(vec![1, 0], &raw, None, true, MIME).await?;
    let status = response.status();
    let body = response.text().await?;
    ensure!(status == StatusCode::OK, "{status}: {body}");
    ensure!(harness.ordered.observed()? == ["model-0"]);
    let control = harness.ordered.control(true, HttpVersion::Http1Only)?;
    let manifest = control
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
        .context("observations")?
        .candidates
    {
        ensure!(
            observed.rate.context("rate")?.shared_count == u64::from(observed.candidate_index == 0)
        );
    }
    {
        let records = harness
            .peer
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("records"))?;
        ensure!(records.len() == 1 && records[0].0.rank == 1);
        ensure!(records[0].1["request"]["cachedContent"] == "cachedContents/native");
    }
    harness.ordered.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_generate_content_mounted_invalid_terminal_tail_retains_unknown_without_fallback()
-> Result<()> {
    for tail in [
        "data: {\"responseId\":\"switched\"}\n\n",
        "event: error\ndata: {\"candidates\":[{\"index\":0,\"finishReason\":\"STOP\"}]}\n\n",
        "data: {\"unfinished\":",
        "data: {\"usageMetadata\":{}}\n",
    ] {
        let harness = Harness::new(1, "messages", false).await?;
        harness.peer.reject.store(false, Ordering::Release);
        let first = json!({"responseId":"original","candidates":[{"index":0,"content":{"parts":[{"text":"prefix"}]}}]});
        let terminal = json!({"candidates":[{"index":0,"finishReason":"STOP"}]});
        *harness
            .peer
            .reply_data
            .lock()
            .map_err(|_| anyhow::anyhow!("reply"))? = Some((
            "text/event-stream".into(),
            format!("data: {first}\n\ndata: {terminal}\n\n{tail}"),
        ));
        ensure!(harness.invoke(true, HttpVersion::Http1Only).await.is_err());
        ensure!(harness.ordered.observed()? == ["model-1"]);
        let id = {
            let records = harness
                .peer
                .records
                .lock()
                .map_err(|_| anyhow::anyhow!("records"))?;
            ensure!(records.len() == 1);
            records[0].0.execution_id.clone()
        };
        let receipt = harness.ordered.receipt(&id).await?;
        ensure!(receipt.outcome != Outcome::Completed && receipt.attempts.len() == 1);
        harness.ordered.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn native_generate_content_mounted_revocation_after_ordinary_send_prevents_native_hop()
-> Result<()> {
    use bitrouter_ai::tenancy::operation::{Mutation, OperationStatus};
    let harness = Harness::new(0, "responses", false).await?;
    let gate = Arc::new(barrier::Gate {
        entered: tokio::sync::Notify::new(),
        release: tokio::sync::Semaphore::new(0),
    });
    *harness
        .ordered
        .held_provider
        .lock()
        .map_err(|_| anyhow::anyhow!("gate"))? = Some(gate.clone());
    let binding = harness
        .ordered
        .binding(vec![1, 0], GenerationProtocol::GenerateContent)
        .await?;
    let control = harness.ordered.control(true, HttpVersion::Http1Only)?;
    let capabilities = control.ordered_generate_content_capabilities().await?;
    let gateway = GatewayClient::new(&harness.ordered.endpoint, None)?;
    let token = harness.ordered.token()?.to_owned();
    let task = AbortOnDropHandle::new(tokio::spawn(async move {
        let request = OrderedGenerateContentRequestV1 {
            schema_version: 1,
            model: MODEL.into(),
            stream: false,
            request: [("contents".into(), json!([{"parts":[{"text":"hello"}]}]))].into(),
        };
        gateway
            .ordered_tenant(&token, &binding)?
            .with_generate_content(&capabilities)?
            .generate_content_reply(&request)
            .await
    }));
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    ensure!(harness.ordered.observed()? == ["model-1"]);
    let revoked = control
        .revoke_credential(
            &harness.ordered.fixture.tenant,
            &harness.ordered.credential.credential_id,
            &Mutation {
                operation_id: "native-revoke-between-hops".try_into()?,
                expected_revision: harness.ordered.credential.credential_revision.clone(),
                installation_id: harness.ordered.fixture.installation.clone(),
            },
        )
        .await?;
    ensure!(revoked.status == OperationStatus::Applied);
    gate.release.add_permits(1);
    let result = tokio::time::timeout(TIMEOUT, task).await??;
    ensure!(result.is_err());
    tokio::time::timeout(
        TIMEOUT,
        harness.ordered.fixture.service.admission.wait_drained(),
    )
    .await?;
    ensure!(harness.ordered.observed()? == ["model-1"]);
    ensure!(
        harness
            .peer
            .records
            .lock()
            .map_err(|_| anyhow::anyhow!("records"))?
            .is_empty()
    );
    let manifest = control
        .candidate_manifest(
            &harness.ordered.fixture.installation,
            &harness.ordered.fixture.tenant,
            &harness.ordered.revision.revision_id,
            &harness.ordered.route,
            GenerationProtocol::GenerateContent,
        )
        .await?;
    for candidate in manifest
        .runtime_observation
        .context("observation")?
        .candidates
    {
        ensure!(
            candidate.rate.context("rate")?.shared_count
                == u64::from(candidate.candidate_index == 1)
        );
    }
    harness.ordered.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn native_generate_content_mounted_drop_after_observed_headers_cancels_one_owner_without_replay()
-> Result<()> {
    for media_only in [false, true] {
        let harness = Harness::new(1, "chat_completions", false).await?;
        harness.peer.reject.store(false, Ordering::Release);
        if media_only {
            let media = json!({"candidates":[{"index":0,"content":{"parts":[{"inlineData":{"mimeType":"image/png","data":"AA=="}}]}}]});
            let terminal = json!({"candidates":[{"index":0,"finishReason":"STOP"}]});
            *harness
                .peer
                .reply_data
                .lock()
                .map_err(|_| anyhow::anyhow!("reply"))? = Some((
                "text/event-stream".into(),
                format!("data: {media}\n\ndata: {terminal}\n\n"),
            ));
        }
        let gate = StreamGate::new();
        *harness
            .peer
            .held_stream
            .lock()
            .map_err(|_| anyhow::anyhow!("gate"))? = Some(gate.clone());
        let binding = harness
            .ordered
            .binding(vec![1, 0], GenerationProtocol::GenerateContent)
            .await?;
        let control = harness.ordered.control(true, HttpVersion::Http1Only)?;
        let capabilities = control.ordered_generate_content_capabilities().await?;
        let gateway = GatewayClient::new(&harness.ordered.endpoint, None)?;
        let request = OrderedGenerateContentRequestV1 {
            schema_version: 1,
            model: MODEL.into(),
            stream: true,
            request: [("contents".into(), json!([{"parts":[{"text":"hello"}]}]))].into(),
        };
        let reply = gateway
            .ordered_tenant(harness.ordered.token()?, &binding)?
            .with_generate_content(&capabilities)?
            .generate_content_reply(&request)
            .await?;
        let id = reply.metadata.execution_id.context("id")?;
        let Body::Stream(mut events) = reply.body else {
            anyhow::bail!("native stream")
        };
        let first = tokio::time::timeout(TIMEOUT, events.next())
            .await?
            .context("first frame")??;
        let StreamItem::Event(first) = first else {
            anyhow::bail!("premature native terminal")
        };
        if media_only {
            ensure!(first.data["candidates"][0]["content"]["parts"][0]["inlineData"].is_object());
            ensure!(
                first.data.get("responseId").is_none() && first.data.get("usageMetadata").is_none()
            );
        }
        tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
        ensure!(!gate.closed.load(Ordering::Acquire));
        drop(events);
        tokio::time::timeout(TIMEOUT, gate.dropped.notified()).await?;
        ensure!(gate.closed.load(Ordering::Acquire));
        tokio::time::timeout(
            TIMEOUT,
            harness.ordered.fixture.service.admission.wait_drained(),
        )
        .await?;
        ensure!(harness.ordered.observed()? == ["model-1"]);
        let receipt = harness.ordered.receipt(&id).await?;
        ensure!(receipt.outcome != Outcome::Completed && receipt.attempts.len() == 1);
        harness.ordered.fixture.service.shutdown().await?;
    }
    Ok(())
}
