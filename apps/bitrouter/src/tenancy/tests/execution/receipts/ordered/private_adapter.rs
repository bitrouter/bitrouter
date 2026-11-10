//! Exercises the paired adapter at the actual profiled JSON and SSE send gates.

use axum::body::Bytes;
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use bitrouter_ai::tenancy::private_adapter::{
    ATTEMPT_DOMAIN, ATTEMPT_HEADER, Attempt, Disposition, OUTCOME_DOMAIN, OUTCOME_HEADER,
    Outcome as AdapterOutcome,
};
use hmac::{Hmac, KeyInit, Mac};
use sha2::{Digest, Sha256};

use super::*;

pub(super) const KEY: [u8; 32] = [19; 32];

#[derive(Clone, Copy)]
enum Reply {
    Signed,
    Missing,
    AlteredRetryAfter,
    Refused,
    Replay,
    EchoBadRequest,
    Redirect,
}

pub(super) struct FixtureAdapter {
    pub(super) ordinal: usize,
    reply: Reply,
    proofs: Mutex<Vec<Attempt>>,
    old_outcome: Mutex<Option<http::HeaderValue>>,
}

pub(super) async fn serve(
    State((state, adapter)): State<(Provider, Arc<FixtureAdapter>)>,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    match reply(state, adapter, uri, headers, body).await {
        Ok(response) => response,
        Err(_) => StatusCode::INTERNAL_SERVER_ERROR.into_response(),
    }
}

async fn reply(
    state: Provider,
    adapter: Arc<FixtureAdapter>,
    uri: http::Uri,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response> {
    let wire = headers.get(ATTEMPT_HEADER).context("attempt")?.to_str()?;
    let (payload, tag) = wire.split_once('.').context("envelope")?;
    let tag = URL_SAFE_NO_PAD.decode(tag)?;
    let payload = URL_SAFE_NO_PAD.decode(payload)?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&KEY)?;
    mac.update(ATTEMPT_DOMAIN);
    mac.update(&payload);
    mac.verify_slice(&tag)
        .map_err(|_| anyhow::anyhow!("attempt MAC"))?;
    let claim: Attempt = serde_json::from_slice(&payload)?;
    let value: Value = serde_json::from_slice(&body)?;
    ensure!(claim.audience == "fixture-peer");
    ensure!(claim.method == "POST");
    ensure!(url::Url::parse(&claim.endpoint)?.path() == uri.path());
    ensure!(claim.body_sha256 == URL_SAFE_NO_PAD.encode(Sha256::digest(&body)));
    ensure!(claim.stream == (value["stream"] == true));
    ensure!(claim.expires_at_ms > u64::try_from(chrono::Utc::now().timestamp_millis())?);
    ensure!(usize::from(claim.ordinal) == adapter.ordinal);
    ensure!(claim.credential.resource_id.as_str() == format!("static-slot-{}", adapter.ordinal));
    let stream = claim.stream;
    adapter
        .proofs
        .lock()
        .map_err(|_| anyhow::anyhow!("proofs"))?
        .push(claim);
    let attempt_wire = wire.to_owned();
    let mut response = ordered_provider(State(state), uri, headers, Json(value)).await;
    if matches!(adapter.reply, Reply::Redirect) {
        return Ok((
            StatusCode::TEMPORARY_REDIRECT,
            [("location", "/v1/chat/completions")],
        )
            .into_response());
    }
    if matches!(adapter.reply, Reply::EchoBadRequest) {
        *response.status_mut() = StatusCode::BAD_REQUEST;
    }
    let status = response.status();
    let retry_after = (status == StatusCode::TOO_MANY_REQUESTS).then_some(3);
    if retry_after.is_some() {
        response
            .headers_mut()
            .insert("retry-after", http::HeaderValue::from_static("3"));
    }
    let disposition = match adapter.reply {
        Reply::Refused => Disposition::Refused,
        _ if !status.is_success() => Disposition::ProviderRejected,
        _ if stream => Disposition::Streaming,
        _ => Disposition::Completed,
    };
    let payload = serde_json::to_vec(&AdapterOutcome {
        disposition,
        status: status.as_u16(),
        retry_after,
    })?;
    let mut mac = Hmac::<Sha256>::new_from_slice(&KEY)?;
    mac.update(OUTCOME_DOMAIN);
    mac.update(&tag);
    mac.update(&payload);
    let value = http::HeaderValue::from_str(&format!(
        "{}.{}",
        URL_SAFE_NO_PAD.encode(payload),
        URL_SAFE_NO_PAD.encode(mac.finalize().into_bytes())
    ))?;
    match adapter.reply {
        Reply::Missing => return Ok(response),
        Reply::AlteredRetryAfter => {
            response
                .headers_mut()
                .insert("retry-after", http::HeaderValue::from_static("1"));
        }
        _ => {}
    }
    let value = if matches!(adapter.reply, Reply::Replay) {
        let mut old = adapter
            .old_outcome
            .lock()
            .map_err(|_| anyhow::anyhow!("outcome"))?;
        old.get_or_insert(value).clone()
    } else {
        value
    };
    if matches!(adapter.reply, Reply::EchoBadRequest) {
        response = (
            status,
            Json(json!({"error":{"message":format!("{attempt_wire} {}", value.to_str()?)}})),
        )
            .into_response();
    }
    response.headers_mut().insert(OUTCOME_HEADER, value);
    Ok(response)
}

async fn setup(ordinal: usize, reply: Reply) -> Result<(OrderedHarness, Arc<FixtureAdapter>)> {
    let adapter = Arc::new(FixtureAdapter {
        ordinal,
        reply,
        proofs: Mutex::new(Vec::new()),
        old_outcome: Mutex::new(None),
    });
    let harness = OrderedHarness::with_fixture_options(
        2,
        8,
        None,
        Value::Null,
        Some(&[profiled::rules(1000, false)?, profiled::rules(1000, false)?]),
        (false, Some(adapter.clone()), CapacityConfig::default()),
        |_| Ok(()),
    )
    .await?;
    harness
        .controls
        .rejection_status
        .store(429, Ordering::SeqCst);
    Ok((harness, adapter))
}

const CHAT: GenerationProtocol = GenerationProtocol::ChatCompletions;

#[cfg(feature = "inference-routing")]
#[tokio::test]
async fn scheduling_missing_adapter_proof_keeps_uncertainty_and_original_order() -> Result<()> {
    for stream in [false, true] {
        let (harness, adapter) = setup(1, Reply::Missing).await?;
        let scheduler = harness.install_scheduler()?;
        let binding = harness.binding(vec![1, 0], CHAT).await?;
        ensure!(
            harness
                .invoke(&binding, CHAT, stream, HttpVersion::Http1Only)
                .await
                .is_err()
        );
        ensure!(harness.observed()? == ["model-1"]);
        let id = adapter
            .proofs
            .lock()
            .map_err(|_| anyhow::anyhow!("proofs"))?
            .first()
            .context("actual claim")?
            .execution_id
            .clone();
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.state == ReceiptState::Unknown && receipt.attempts.len() == 1);
        let prompt: bitrouter_ai::types::Prompt =
            serde_json::from_value(json!({"model":"model-1","messages":[],"stream":stream}))?;
        let (shape, _) = crate::gateway::scheduling::observation::Shape::from_prompt(&prompt)?;
        let view = scheduler.inspect(shape);
        ensure!(view[1].uncertain == 1 && view[1].counters.rejected == 0 && view[0].work == 0);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[cfg(feature = "inference-routing")]
#[tokio::test]
async fn scheduling_verified_rejection_releases_only_its_original_pool() -> Result<()> {
    for stream in [false, true] {
        let (harness, _) = setup(1, Reply::Signed).await?;
        let scheduler = harness.install_scheduler()?;
        let binding = harness.binding(vec![1, 0], CHAT).await?;
        harness
            .invoke(&binding, CHAT, stream, HttpVersion::Http1Only)
            .await?;
        ensure!(harness.observed()? == ["model-1", "model-0"]);
        let prompt: bitrouter_ai::types::Prompt =
            serde_json::from_value(json!({"model":"model-1","messages":[],"stream":stream}))?;
        let (shape, _) = crate::gateway::scheduling::observation::Shape::from_prompt(&prompt)?;
        let view = scheduler.inspect(shape);
        ensure!(
            view[1].counters.rejected == 1 && view[1].work == 0 && view[0].counters.completed == 1
        );
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn signed_adapter_rejection_advances_original_order_and_regular_target_gets_no_claim()
-> Result<()> {
    for stream in [false, true] {
        let (harness, adapter) = setup(1, Reply::Signed).await?;
        let binding = harness.binding(vec![1, 0], CHAT).await?;
        let id = harness
            .invoke(&binding, CHAT, stream, HttpVersion::Http1Only)
            .await?;
        let receipt = harness.receipt(&id).await?;
        ensure!(receipt.outcome == Outcome::Completed && receipt.attempts.len() == 2);
        ensure!(harness.observed()? == ["model-1", "model-0"]);
        {
            let proofs = adapter
                .proofs
                .lock()
                .map_err(|_| anyhow::anyhow!("proofs"))?;
            let claim = proofs.first().context("actual claim")?;
            ensure!(proofs.len() == 1 && claim.ordinal == 1 && claim.rank == 0);
            ensure!(claim.execution_id == id && claim.attempt_id == receipt.attempts[0].attempt_id);
            ensure!(claim.tenant_id == harness.fixture.tenant && claim.route == harness.route);
            ensure!(claim.correlation_id == binding.correlation_id);
            let observed = harness
                .observed
                .lock()
                .map_err(|_| anyhow::anyhow!("observed"))?;
            ensure!(!observed[1].1.contains_key(ATTEMPT_HEADER));
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn unverified_adapter_rejections_leave_an_unknown_receipt_and_stop_before_fallback()
-> Result<()> {
    for stream in [false, true] {
        for reply in [
            Reply::Missing,
            Reply::AlteredRetryAfter,
            Reply::Refused,
            Reply::Redirect,
        ] {
            let (harness, adapter) = setup(1, reply).await?;
            let binding = harness.binding(vec![1, 0], CHAT).await?;
            ensure!(
                harness
                    .invoke(&binding, CHAT, stream, HttpVersion::Http1Only)
                    .await
                    .is_err()
            );
            ensure!(harness.observed()? == ["model-1"]);
            let id = {
                let proofs = adapter
                    .proofs
                    .lock()
                    .map_err(|_| anyhow::anyhow!("proofs"))?;
                proofs.first().context("actual claim")?.execution_id.clone()
            };
            let receipt = harness.receipt(&id).await?;
            ensure!(receipt.state == ReceiptState::Unknown && receipt.attempts.len() == 1);
            harness.fixture.service.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn final_signed_rejection_scrubs_both_envelopes_in_json_and_stream_requests() -> Result<()> {
    for stream in [false, true] {
        let (harness, _) = setup(1, Reply::EchoBadRequest).await?;
        let binding = harness.binding(vec![1], CHAT).await?;
        let client = GatewayClient::new(&harness.endpoint, None)?;
        let scoped = client.ordered_tenant(harness.token()?, &binding)?;
        let request = serde_json::from_value(json!({
            "model": MODEL, "messages": [{"role":"user","content":"hello"}], "stream": stream,
        }))?;
        let Err(bitrouter_sdk::gateway::tenancy::Error::Response(failure)) =
            scoped.chat_reply(&request).await
        else {
            anyhow::bail!("expected a definitive HTTP error");
        };
        ensure!(failure.metadata.status == 400);
        let detail = serde_json::to_string(&failure.gateway_error.context("gateway error")?)?;
        ensure!(detail.contains("[redacted adapter envelope] [redacted adapter envelope]"));
        ensure!(harness.observed()? == ["model-1"]);
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn valid_stream_headers_and_terminal_pass_but_replayed_outcome_cannot_complete_new_send()
-> Result<()> {
    let (harness, adapter) = setup(0, Reply::Replay).await?;
    let binding = harness.binding(vec![0, 1], CHAT).await?;
    let id = harness
        .invoke(&binding, CHAT, true, HttpVersion::Http1Only)
        .await?;
    ensure!(harness.receipt(&id).await?.outcome == Outcome::Completed);
    ensure!(
        harness
            .invoke(&binding, CHAT, true, HttpVersion::Http1Only)
            .await
            .is_err()
    );
    ensure!(harness.observed()? == ["model-0", "model-0"]);
    let second = {
        let proofs = adapter
            .proofs
            .lock()
            .map_err(|_| anyhow::anyhow!("proofs"))?;
        ensure!(proofs.len() == 2 && proofs[0].execution_id != proofs[1].execution_id);
        ensure!(proofs[0].correlation_id == proofs[1].correlation_id);
        proofs[1].execution_id.clone()
    };
    ensure!(harness.receipt(&second).await?.state == ReceiptState::Unknown);
    harness.fixture.service.shutdown().await?;
    Ok(())
}
