//! Real tenant execution with the existing exporter and an in-process sink.
use super::*;
use crate::telemetry::exporter::{OtelExporter, hop_tests::make_test_exporter};
use opentelemetry::trace::SpanKind;
use opentelemetry_sdk::trace::SpanData;

fn attribute(span: &SpanData, key: &str) -> Option<String> {
    span.attributes
        .iter()
        .find(|attr| attr.key.as_str() == key)
        .map(|attr| attr.value.to_string())
}

fn bind(harness: &OrderedHarness) -> (Arc<OtelExporter>, Arc<Mutex<Vec<SpanData>>>) {
    let (exporter, captured) = make_test_exporter();
    let exporter = Arc::new(exporter);
    harness
        .fixture
        .service
        .bind_operational_exporter(exporter.clone());
    (exporter, captured)
}

#[cfg(feature = "otel-http")]
#[tokio::test]
async fn actual_otlp_http_rejection_cannot_block_dispatch_or_durable_readback() -> Result<()> {
    use crate::telemetry::{
        config::{OtelConfig, SamplerKind},
        exporter::hop_tests::make_http_test_exporter,
    };
    let rejected = Arc::new(tokio::sync::Notify::new());
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let endpoint = format!("http://{}", listener.local_addr()?);
    let app = Router::new()
        .route(
            "/v1/traces",
            post(
                |State(rejected): State<Arc<tokio::sync::Notify>>| async move {
                    rejected.notify_one();
                    StatusCode::BAD_REQUEST
                },
            ),
        )
        .with_state(rejected.clone());
    let _collector =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, app).await },
        ));
    let mut config = OtelConfig {
        endpoint,
        sampler: SamplerKind::AlwaysOn,
        ..Default::default()
    };
    config.metrics.enabled = false;
    config.traces.batch.flush_ms = 1;
    let exporter = Arc::new(make_http_test_exporter(config)?);
    let harness =
        OrderedHarness::with_profiles(1, 8, None, Value::Null, Some(&[rules(10, true)?])).await?;
    harness
        .fixture
        .service
        .bind_operational_exporter(exporter.clone());
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    let id = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    ensure!(harness.receipt(&id).await?.outcome == Outcome::Completed);
    tokio::time::timeout(TIMEOUT, rejected.notified())
        .await
        .context("OTLP exporter did not reach rejecting local collector")?;
    let second = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    ensure!(harness.receipt(&second).await?.outcome == Outcome::Completed && second != id);
    ensure!(harness.observed()?.len() == 2);
    harness.fixture.service.shutdown().await?;
    tokio::task::spawn_blocking(move || exporter.shutdown()).await?;
    Ok(())
}

#[tokio::test]
async fn caller_drops_sse_before_terminal_retains_unknown_one_hop_without_resend() -> Result<()> {
    let harness = OrderedHarness::with_profiles(
        2,
        8,
        None,
        Value::Null,
        Some(&[rules(1000, true)?, rules(1000, true)?]),
    )
    .await?;
    let (exporter, captured) = bind(&harness);
    let gate = Arc::new(barrier::Gate {
        entered: tokio::sync::Notify::new(),
        release: Semaphore::new(0),
    });
    *harness
        .controls
        .held_stream
        .lock()
        .map_err(|_| anyhow::anyhow!("stream gate"))? = Some(gate.clone());
    let binding = harness
        .binding(vec![0, 1], GenerationProtocol::ChatCompletions)
        .await?;
    let client = GatewayClient::new(&harness.endpoint, None)?;
    let request: ChatRequest = serde_json::from_value(
        json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":true}),
    )?;
    let reply = client
        .ordered_tenant(harness.token()?, &binding)?
        .chat_reply(&request)
        .await?;
    let id = reply.metadata.execution_id.context("execution identity")?;
    tokio::time::timeout(TIMEOUT, gate.entered.notified()).await?;
    ensure!(exporter.test_expire_spans()? == 1);
    drop(reply.body);
    // Keep the provider terminal unavailable until the actual disconnect has
    // released its journal permit. A late terminal must not certify this call.
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let receipt = harness.receipt(&id).await?;
    ensure!(
        receipt.state == ReceiptState::Unknown
            && receipt.outcome == Outcome::Unknown
            && receipt.attempts.len() == 1
            && receipt.attempts[0].candidate_index == 0
            && receipt.attempts[0].outcome == Outcome::Unknown
            && receipt.attempts[0].observed_http_status == Some(200),
        "unexpected disconnected receipt: {receipt:?}"
    );
    ensure!(harness.observed()? == ["model-0"]);
    let spans = captured
        .lock()
        .map_err(|_| anyhow::anyhow!("capture"))?
        .clone();
    let hops: Vec<_> = spans
        .iter()
        .filter(|span| span.span_kind == SpanKind::Client)
        .collect();
    ensure!(
        hops.len() == 1
            && attribute(hops[0], "bitrouter.attempt_id")
                == Some(receipt.attempts[0].attempt_id.to_string())
    );
    ensure!(attribute(hops[0], "bitrouter.hop_outcome").as_deref() == Some("caller_cancelled"));
    ensure!(attribute(hops[0], "http.response.status_code").as_deref() == Some("200"));
    ensure!(exporter.test_expire_spans()? == 0);
    let observation = manifest(&harness)
        .await?
        .runtime_observation
        .context("observation")?;
    ensure!(
        observation
            .candidates
            .iter()
            .all(|candidate| candidate.health == HealthObservation::Healthy)
    );
    ensure!(
        observation.candidates[0]
            .rate
            .as_ref()
            .context("rate")?
            .shared_count
            == 1
    );
    gate.release.add_permits(1);
    ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == serde_json::to_vec(&receipt)?);
    ensure!(harness.observed()? == ["model-0"]);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn caller_drops_unread_sse_after_durable_terminal_preserves_completed_one_hop() -> Result<()>
{
    let harness = OrderedHarness::with_profiles(
        2,
        8,
        None,
        Value::Null,
        Some(&[rules(1000, true)?, rules(1000, true)?]),
    )
    .await?;
    let (exporter, captured) = bind(&harness);
    let binding = harness
        .binding(vec![0, 1], GenerationProtocol::ChatCompletions)
        .await?;
    let client = GatewayClient::new(&harness.endpoint, None)?;
    let request: ChatRequest = serde_json::from_value(
        json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"stream":true}),
    )?;
    let reply = client
        .ordered_tenant(harness.token()?, &binding)?
        .chat_reply(&request)
        .await?;
    let id = reply.metadata.execution_id.context("execution identity")?;
    // The finite provider reply fits the existing delivery buffer. Wait for
    // actual durable completion without polling or dropping the caller body.
    tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
    let before = harness.receipt(&id).await?;
    ensure!(before.outcome == Outcome::Completed && before.attempts.len() == 1);
    ensure!(
        before.attempts[0].candidate_index == 0
            && before.attempts[0].observed_http_status == Some(200)
    );
    drop(reply.body);
    ensure!(serde_json::to_vec(&harness.receipt(&id).await?)? == serde_json::to_vec(&before)?);
    ensure!(harness.observed()? == ["model-0"]);
    let spans = captured
        .lock()
        .map_err(|_| anyhow::anyhow!("capture"))?
        .clone();
    let hops: Vec<_> = spans
        .iter()
        .filter(|span| span.span_kind == SpanKind::Client)
        .collect();
    ensure!(
        hops.len() == 1
            && attribute(hops[0], "bitrouter.hop_outcome").as_deref() == Some("completed")
    );
    ensure!(exporter.test_expire_spans()? == 0);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn trusted_tenant_hops_report_actual_ordinal_attempt_status_and_local_absence() -> Result<()>
{
    let profiles = [rules(1000, false)?, rules(1000, false)?, rules(0, false)?];
    let harness = OrderedHarness::with_profiles(3, 8, None, Value::Null, Some(&profiles)).await?;
    let (_exporter, captured) = bind(&harness);
    let binding = harness
        .binding(vec![2, 1, 0], GenerationProtocol::ChatCompletions)
        .await?;
    let id = harness
        .invoke(
            &binding,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
        )
        .await?;
    let receipt = harness.receipt(&id).await?;
    let spans = captured
        .lock()
        .map_err(|_| anyhow::anyhow!("capture lock"))?
        .clone();
    let hops: Vec<_> = spans
        .iter()
        .filter(|span| span.span_kind == SpanKind::Client)
        .collect();
    ensure!(
        hops.len() == 3,
        "expected three actual reached hops, got {}",
        hops.len()
    );
    for (hop, index) in hops.iter().zip([2, 1, 0]) {
        ensure!(attribute(hop, "bitrouter.candidate_index") == Some(index.to_string()));
        ensure!(
            attribute(hop, "bitrouter.installation_id")
                == Some(harness.fixture.installation.to_string())
        );
        ensure!(attribute(hop, "bitrouter.tenant_id") == Some(harness.fixture.tenant.to_string()));
        ensure!(attribute(hop, "bitrouter.request_id") == Some(id.to_string()));
        ensure!(
            attribute(hop, "bitrouter.execution_revision")
                == Some(harness.revision.revision_id.to_string())
        );
        ensure!(
            attribute(hop, "bitrouter.inbound_protocol").as_deref() == Some("chat_completions")
        );
        ensure!(attribute(hop, "bitrouter.hop_duration_ms").is_some());
    }
    ensure!(attribute(hops[0], "bitrouter.hop_outcome").as_deref() == Some("local_rate_limited"));
    ensure!(attribute(hops[0], "http.response.status_code").is_none());
    ensure!(attribute(hops[0], "bitrouter.attempt_id").is_none());
    for (hop, attempt) in hops[1..].iter().zip(&receipt.attempts) {
        ensure!(attribute(hop, "bitrouter.attempt_id") == Some(attempt.attempt_id.to_string()));
    }
    ensure!(attribute(hops[1], "http.response.status_code").as_deref() == Some("503"));
    ensure!(attribute(hops[1], "bitrouter.hop_outcome").as_deref() == Some("http_rejection"));
    ensure!(attribute(hops[2], "http.response.status_code").as_deref() == Some("200"));
    ensure!(attribute(hops[2], "bitrouter.hop_outcome").as_deref() == Some("completed"));
    let root = spans
        .iter()
        .find(|span| {
            span.span_kind == SpanKind::Internal
                && attribute(span, "bitrouter.request_id") == Some(id.to_string())
        })
        .context("root span")?;
    ensure!(attribute(root, "bitrouter.candidate_index").as_deref() == Some("0"));
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn invalid_success_keeps_actual_200_instead_of_gateway_failure_status() -> Result<()> {
    let harness =
        OrderedHarness::with_profiles(1, 8, Some(0), Value::Null, Some(&[rules(1000, true)?]))
            .await?;
    let (_exporter, captured) = bind(&harness);
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    ensure!(
        harness
            .invoke(
                &binding,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only
            )
            .await
            .is_err()
    );
    let spans = captured
        .lock()
        .map_err(|_| anyhow::anyhow!("capture lock"))?
        .clone();
    let hop = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Client)
        .context("hop span")?;
    ensure!(attribute(hop, "http.response.status_code").as_deref() == Some("200"));
    ensure!(attribute(hop, "bitrouter.hop_outcome").as_deref() == Some("protocol_invalid"));
    ensure!(attribute(hop, "bitrouter.attempt_id").is_some());
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn live_tenant_stream_survives_span_gc_and_finishes_after_real_terminal() -> Result<()> {
    let harness =
        OrderedHarness::with_profiles(1, 8, None, Value::Null, Some(&[rules(1000, true)?])).await?;
    let (exporter, captured) = bind(&harness);
    let gate = Arc::new(barrier::Gate {
        entered: tokio::sync::Notify::new(),
        release: Semaphore::new(0),
    });
    *harness
        .controls
        .held_stream
        .lock()
        .map_err(|_| anyhow::anyhow!("stream gate lock"))? = Some(gate.clone());
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    let request = harness.invoke(
        &binding,
        GenerationProtocol::ChatCompletions,
        true,
        HttpVersion::Http1Only,
    );
    tokio::pin!(request);
    tokio::select! {
        result = &mut request => anyhow::bail!("stream finished before barrier: {result:?}"),
        entered = tokio::time::timeout(TIMEOUT, gate.entered.notified()) => { entered?; }
    }
    // Drive the actual GC beyond its 300-second age threshold without sleeping
    // or changing request deadlines. The owner remains weak in the exporter.
    ensure!(exporter.test_expire_spans()? == 1);
    let before = captured
        .lock()
        .map_err(|_| anyhow::anyhow!("capture lock"))?
        .clone();
    ensure!(before.len() == 1 && before[0].name == "route");
    ensure!(
        before[0].span_kind == SpanKind::Internal
            && attribute(&before[0], "bitrouter.request_id").is_none()
    );
    gate.release.add_permits(1);
    let id = request.await?;
    ensure!(harness.receipt(&id).await?.outcome == Outcome::Completed);
    let spans = captured
        .lock()
        .map_err(|_| anyhow::anyhow!("capture lock"))?
        .clone();
    let hop = spans
        .iter()
        .find(|span| span.span_kind == SpanKind::Client)
        .context("stream hop")?;
    ensure!(
        spans
            .iter()
            .filter(|span| span.span_kind == SpanKind::Client)
            .count()
            == 1
    );
    let roots: Vec<_> = spans
        .iter()
        .filter(|span| {
            span.span_kind == SpanKind::Internal
                && attribute(span, "bitrouter.request_id") == Some(id.to_string())
        })
        .collect();
    ensure!(roots.len() == 1);
    let root = roots[0];
    ensure!(hop.parent_span_id == root.span_context.span_id());
    ensure!(before[0].parent_span_id == root.span_context.span_id());
    ensure!(
        hop.span_context.trace_id() == root.span_context.trace_id()
            && before[0].span_context.trace_id() == root.span_context.trace_id()
    );
    ensure!(attribute(hop, "bitrouter.hop_outcome").as_deref() == Some("completed"));
    ensure!(attribute(hop, "bitrouter.request_id") == Some(id.to_string()));
    ensure!(exporter.test_expire_spans()? == 0);
    harness.fixture.service.shutdown().await?;
    Ok(())
}
