use super::*;
use bitrouter_ai::tenancy::generate_content::{
    self, OrderedGenerateContentCapabilitiesV1, OrderedGenerateContentRequestV1,
};

pub(super) fn caps() -> TestResult<OrderedGenerateContentCapabilitiesV1> {
    Ok(serde_json::from_value(json!({
        "contract_version":1,"installation_id":"installation","instance_id":"instance",
        "capability_revision":"capability","authority_revision":7,"enabled":true,"cached_content_enabled":false,
        "path":generate_content::PATH,"request_media_type":generate_content::MIME
    }))?)
}

pub(super) fn input(stream: bool) -> TestResult<OrderedGenerateContentRequestV1> {
    Ok(serde_json::from_value(
        json!({"schema_version":1,"model":"route","stream":stream,
        "request":{"contents":[{"role":"user","parts":[{"text":"native prompt"}]}],
            "cachedContent":null,"generationConfig":{"temperature":0.0}}}),
    )?)
}

pub(super) fn native_response(stream: bool) -> TestResult<Response> {
    let value = json!({"responseId":"native-response","candidates":[{"index":0,
        "content":{"role":"model","parts":[{"text":"answer"}]},"finishReason":"STOP"}],
        "usageMetadata":{"promptTokenCount":4,"candidatesTokenCount":2}});
    Ok(Response::builder()
        .header("x-bitrouter-request-id", "execution")
        .header(
            "content-type",
            if stream {
                "text/event-stream"
            } else {
                "application/json"
            },
        )
        .body(HttpBody::from(if stream {
            format!("data: {value}\n\n")
        } else {
            value.to_string()
        }))?)
}

#[tokio::test]
async fn explicit_native_discovery_and_json_sse_keep_h1_h2_binding_and_media() -> TestResult {
    for (version, wire) in versions() {
        for streaming in [false, true] {
            let server = server::serve(
                wire,
                vec![
                    Some(server::json(serde_json::to_value(caps()?)?)?),
                    Some(native_response(streaming)?),
                    Some(reply(0, false)?),
                ],
            )
            .await?;
            let observed = control_client(&server, version)?
                .ordered_generate_content_capabilities()
                .await?;
            let client = gateway_client(&server, version)?;
            let binding = binding(vec![255, 3, 0])?;
            let request = input(streaming)?;
            let reply = client
                .ordered_tenant("scoped-token", &binding)?
                .with_generate_content(&observed)?
                .generate_content_reply(&request)
                .await?;
            assert_eq!(
                reply.metadata.execution_id.as_ref().map(|id| id.as_str()),
                Some("execution")
            );
            match reply.body {
                Body::Json(value) => {
                    assert!(!streaming);
                    assert_eq!(
                        value["candidates"][0]["content"]["parts"][0]["text"],
                        "answer"
                    );
                }
                Body::Stream(events) => {
                    assert!(streaming);
                    assert_eq!(end(events).await?, ProtocolEnd::GenerateContentFinished);
                }
            }
            dispatch(&client, &binding, 0, false).await?;
            let records = server.records.lock().await;
            assert_eq!(records.len(), 3);
            assert_eq!(
                records[0].path,
                "/control/v1/tenancy/ordered/generate-content/capabilities"
            );
            assert_eq!(records[0].headers["authorization"], "Bearer operator-token");
            assert_eq!(
                records[1].path,
                "/prefix/v1/tenancy/ordered/generate-content"
            );
            assert_eq!(records[1].version, wire);
            assert_eq!(records[1].headers["authorization"], "Bearer scoped-token");
            assert_eq!(records[1].headers["content-type"], generate_content::MIME);
            assert_eq!(records[1].headers.get_all("content-type").iter().count(), 1);
            assert_eq!(
                serde_json::from_slice::<Value>(&records[1].body)?,
                serde_json::to_value(request)?
            );
            assert_eq!(
                decode_selection(records[1].headers["bitrouter-candidate-selection"].to_str()?)?["candidate_indices"],
                json!([255, 3, 0])
            );
            assert_eq!(records[2].headers["content-type"], "application/json");
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_opt_in_and_bound_defaults_refuse_mismatches_before_send() -> TestResult {
    use bitrouter_ai::tenancy::generation_defaults::CanonicalGenerationDefaultsV1;

    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = gateway_client(&server, HttpVersion::Http1Only)?;
    let mut binding = binding(vec![0])?;
    let request = input(false)?;
    assert!(
        client
            .ordered_tenant("scoped-token", &binding)?
            .generate_content_reply(&request)
            .await
            .is_err()
    );
    for field in [
        "enabled",
        "installation_id",
        "instance_id",
        "capability_revision",
        "contract_version",
        "path",
        "request_media_type",
    ] {
        let mut value = serde_json::to_value(caps()?)?;
        value[field] = match field {
            "enabled" => json!(false),
            "contract_version" => json!(2),
            _ => json!("wrong"),
        };
        assert!(
            client
                .ordered_tenant("scoped-token", &binding)?
                .with_generate_content(&serde_json::from_value(value)?)
                .is_err()
        );
    }
    for field in [
        "model",
        "stream",
        "bitrouter_generation_defaults",
        "bitrouter_generation_defaults_envelope",
    ] {
        let mut request = input(false)?;
        request.request.insert(field.into(), json!(null));
        assert!(
            client
                .ordered_tenant("scoped-token", &binding)?
                .with_generate_content(&caps()?)?
                .generate_content_reply(&request)
                .await
                .is_err()
        );
    }
    let mut wrong = input(false)?;
    wrong.model = "different-route".into();
    assert!(
        client
            .ordered_tenant("scoped-token", &binding)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&wrong)
            .await
            .is_err()
    );
    let defaults: CanonicalGenerationDefaultsV1 =
        serde_json::from_value(json!({"schema_version":1,
        "max_tokens":8}))?;
    binding.selection.generation_defaults_digest = Some(defaults.digest()?);
    assert!(
        client
            .ordered_tenant("scoped-token", &binding)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&request)
            .await
            .is_err()
    );
    assert!(server.records.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn native_bound_defaults_keep_dedicated_request_content_type() -> TestResult {
    use bitrouter_ai::tenancy::generation_defaults::{BODY_MEMBER, CanonicalGenerationDefaultsV1};
    for (version, wire) in versions() {
        let server = server::serve(wire, vec![Some(native_response(false)?)]).await?;
        let client = gateway_client(&server, version)?;
        let defaults: CanonicalGenerationDefaultsV1 =
            serde_json::from_value(json!({"schema_version":1,
            "max_tokens":8}))?;
        let mut binding = binding(vec![0])?;
        binding.selection.generation_defaults_digest = Some(defaults.digest()?);
        client
            .ordered_tenant("scoped-token", &binding)?
            .with_generate_content(&caps()?)?
            .with_generation_defaults(defaults.clone())?
            .generate_content_reply(&input(false)?)
            .await?;
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].headers["content-type"], generate_content::MIME);
        assert_eq!(records[0].headers.get_all("content-type").iter().count(), 1);
        let body: Value = serde_json::from_slice(&records[0].body)?;
        assert_eq!(body[BODY_MEMBER], serde_json::to_value(defaults)?);
        assert_eq!(body["request"]["cachedContent"], Value::Null);
    }
    Ok(())
}

#[tokio::test]
async fn native_finish_waits_for_clean_eof_and_retains_late_usage() -> TestResult {
    use bytes::Bytes;
    use std::convert::Infallible;
    use tokio::sync::oneshot;
    for (version, wire) in versions() {
        let (release, held) = oneshot::channel::<()>();
        let body = async_stream::stream! {
            yield Ok::<_, Infallible>(Bytes::from_static(b"data: {\"responseId\":\"r\",\"candidates\":[{\"index\":0,\"finishReason\":\"STOP\"}]}\n\n"));
            let _ = held.await;
            yield Ok(Bytes::from_static(b"data: {\"usageMetadata\":{\"promptTokenCount\":4,\"candidatesTokenCount\":2}}\n\n"));
        };
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "text/event-stream")
            .body(HttpBody::from_stream(body))?;
        let server = server::serve(wire, vec![Some(response)]).await?;
        let client = gateway_client(&server, version)?;
        let reply = client
            .ordered_tenant("scoped-token", &binding(vec![0])?)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&input(true)?)
            .await?;
        let Body::Stream(mut events) = reply.body else {
            return Err("expected SSE".into());
        };
        let next = events.next();
        tokio::pin!(next);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut next)
                .await
                .is_err()
        );
        release
            .send(())
            .map_err(|_| "fixture closed before EOF release")?;
        let first = next.await.ok_or("missing held native finish")??;
        assert!(matches!(first, StreamItem::Event(ref event)
            if event.data["candidates"][0]["finishReason"] == "STOP"));
        assert!(
            matches!(events.next().await.ok_or("missing late usage")??, StreamItem::Event(ref event)
            if event.data["usageMetadata"]["promptTokenCount"] == 4)
        );
        assert!(matches!(
            events.next().await.ok_or("missing explicit end")??,
            StreamItem::End(ProtocolEnd::GenerateContentFinished)
        ));
        assert!(events.next().await.is_none());
    }
    Ok(())
}

#[tokio::test]
async fn native_invalid_tail_identity_and_missing_finish_never_publish_end() -> TestResult {
    let finish =
        "data: {\"responseId\":\"r\",\"candidates\":[{\"index\":0,\"finishReason\":\"STOP\"}]}\n\n";
    let cases = vec![
        String::new(),
        "data: {\"candidates\":[{\"content\":{\"parts\":[{\"text\":\"incomplete\"}]}}]}\n\n".into(),
        format!("{finish}data: {{bad}}\n\n"),
        format!("{finish}data: {{\"usageMetadata\":"),
        format!("{finish}data: {{\"responseId\":\"changed\"}}\n\n"),
        format!("{finish}data: {{\"error\":{{\"code\":500,\"message\":\"late failure\"}}}}\n\n"),
        format!(
            "{finish}data: {{\"candidates\":[{{\"index\":1,\"content\":{{\"parts\":[{{\"text\":\"unfinished other candidate\"}}]}}}}]}}\n\n"
        ),
        format!(
            "{finish}data: {{\"candidates\":[{{\"content\":{{\"parts\":[{{\"text\":\"after\"}}]}}}}]}}\n\n"
        ),
        format!("{finish}data: {{\"candidates\":[{{\"finishReason\":\"MAX_TOKENS\"}}]}}\n\n"),
        format!(
            "{finish}data: {{\"candidates\":[{{\"finishReason\":\"FINISH_REASON_UNSPECIFIED\"}}]}}\n\n"
        ),
        format!(
            "{finish}event: error\ndata: {{\"error\":{{\"message\":\"late failure\",\"type\":\"upstream_error\",\"code\":\"error\"}}}}\n\n"
        ),
        "data: {\"candidates\":[null]}\n\n".into(),
        "data: {\"candidates\":[{},{}]}\n\n".into(),
        "data: []\n\n".into(),
    ];
    for (version, wire) in versions() {
        for body in &cases {
            let response = Response::builder()
                .header("x-bitrouter-request-id", "execution")
                .header("content-type", "text/event-stream")
                .body(HttpBody::from(body.clone()))?;
            let server = server::serve(wire, vec![Some(response)]).await?;
            let client = gateway_client(&server, version)?;
            let reply = client
                .ordered_tenant("scoped-token", &binding(vec![0])?)?
                .with_generate_content(&caps()?)?
                .generate_content_reply(&input(true)?)
                .await?;
            let Body::Stream(mut events) = reply.body else {
                return Err("expected SSE".into());
            };
            let mut failed = false;
            while let Some(event) = events.next().await {
                match event {
                    Err(_) => {
                        failed = true;
                        break;
                    }
                    Ok(StreamItem::End(_)) => {
                        return Err("invalid native tail published an end".into());
                    }
                    Ok(StreamItem::Event(event)) => {
                        assert!(event.data["candidates"][0]["finishReason"].is_null())
                    }
                }
            }
            assert!(failed, "invalid native stream ended without failure");
            assert_eq!(server.records.lock().await.len(), 1);
        }
    }
    Ok(())
}

#[tokio::test]
async fn native_finish_then_transport_reset_keeps_identity_without_publishing_end() -> TestResult {
    use bytes::Bytes;
    use tokio::sync::oneshot;

    for (version, wire) in versions() {
        let (release, held) = oneshot::channel::<()>();
        let body = async_stream::stream! {
            yield Ok::<_, std::io::Error>(Bytes::from_static(b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n"));
            let _ = held.await;
            yield Err(std::io::Error::other("fixture transport reset"));
        };
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "text/event-stream")
            .body(HttpBody::from_stream(body))?;
        let server = server::serve(wire, vec![Some(response)]).await?;
        let client = gateway_client(&server, version)?;
        let reply = client
            .ordered_tenant("scoped-token", &binding(vec![0])?)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&input(true)?)
            .await?;
        let Body::Stream(mut events) = reply.body else {
            return Err("expected SSE".into());
        };
        let next = events.next();
        tokio::pin!(next);
        assert!(
            tokio::time::timeout(Duration::from_millis(25), &mut next)
                .await
                .is_err()
        );
        release.send(()).map_err(|_| "fixture reset owner closed")?;
        let error = next
            .await
            .ok_or("missing transport reset")?
            .err()
            .ok_or("reset published an event")?;
        let Error::Response(failure) = error else {
            return Err("reset lost response metadata".into());
        };
        assert_eq!(
            failure.metadata.execution_id.as_ref().map(|id| id.as_str()),
            Some("execution")
        );
        assert!(matches!(failure.problem, Some(Problem::Transport)));
        assert!(events.next().await.is_none());
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn dropping_native_stream_before_eof_closes_transport_without_retry() -> TestResult {
    use bytes::Bytes;
    use std::convert::Infallible;
    use tokio::sync::oneshot;

    struct Closed(Option<oneshot::Sender<()>>);
    impl Drop for Closed {
        fn drop(&mut self) {
            if let Some(signal) = self.0.take() {
                let _ = signal.send(());
            }
        }
    }
    for (version, wire) in versions() {
        let (closed, observed) = oneshot::channel::<()>();
        let guard = Closed(Some(closed));
        let body = async_stream::stream! {
            let _guard = guard;
            yield Ok::<_, Infallible>(Bytes::from_static(b"data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n"));
            std::future::pending::<()>().await;
        };
        let response = Response::builder()
            .header("x-bitrouter-request-id", "execution")
            .header("content-type", "text/event-stream")
            .body(HttpBody::from_stream(body))?;
        let server = server::serve(wire, vec![Some(response)]).await?;
        let client = gateway_client(&server, version)?;
        let reply = client
            .ordered_tenant("scoped-token", &binding(vec![0])?)?
            .with_generate_content(&caps()?)?
            .generate_content_reply(&input(true)?)
            .await?;
        let Body::Stream(mut events) = reply.body else {
            return Err("expected SSE".into());
        };
        {
            let next = events.next();
            tokio::pin!(next);
            assert!(
                tokio::time::timeout(Duration::from_millis(25), &mut next)
                    .await
                    .is_err()
            );
        }
        drop(events);
        tokio::time::timeout(TIMEOUT, observed).await??;
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}

#[tokio::test]
async fn native_retained_finish_and_usage_share_event_byte_budget() -> TestResult {
    let mut body = "data: {\"candidates\":[{\"finishReason\":\"STOP\"}]}\n\n".to_owned();
    for _ in 0..8 {
        body.push_str(&format!(
            "data: {{\"usageMetadata\":{{\"opaque\":\"{}\"}}}}\n\n",
            "x".repeat(200)
        ));
    }
    let response = Response::builder()
        .header("x-bitrouter-request-id", "execution")
        .header("content-type", "text/event-stream")
        .body(HttpBody::from(body))?;
    let server = server::serve(Version::HTTP_11, vec![Some(response)]).await?;
    let client = GatewayClient::with_options(
        &server.endpoint,
        None,
        TransportOptions {
            http_version: HttpVersion::Http1Only,
            max_event_bytes: 1024,
            ..Default::default()
        },
    )?;
    let reply = client
        .ordered_tenant("scoped-token", &binding(vec![0])?)?
        .with_generate_content(&caps()?)?
        .generate_content_reply(&input(true)?)
        .await?;
    let Body::Stream(mut events) = reply.body else {
        return Err("expected SSE".into());
    };
    assert!(
        events
            .next()
            .await
            .ok_or("missing terminal retention failure")?
            .is_err()
    );
    assert!(events.next().await.is_none());
    assert_eq!(server.records.lock().await.len(), 1);
    Ok(())
}
