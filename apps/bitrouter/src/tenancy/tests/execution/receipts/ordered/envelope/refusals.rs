use super::*;
use bitrouter_ai::tenancy::{admission::AdmissionProfileV1, operation::ErrorCode};

fn request(protocol: GenerationProtocol) -> Result<(&'static str, Value)> {
    Ok(match protocol {
        GenerationProtocol::GenerateContent => {
            anyhow::bail!("native protocol requires the paired refusal fixture")
        }
        GenerationProtocol::ChatCompletions => (
            "chat/completions",
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}]}),
        ),
        GenerationProtocol::Messages => (
            "messages",
            json!({"model":MODEL,"messages":[{"role":"user","content":"hello"}],"max_tokens":25}),
        ),
        GenerationProtocol::Responses => ("responses", json!({"model":MODEL,"input":"hello"})),
    })
}

async fn send(
    harness: &OrderedHarness,
    binding: &OrderedDispatchBindingV2,
    path: &str,
    raw: String,
) -> Result<reqwest::Response> {
    Ok(reqwest::Client::new()
        .post(format!("{}/v1/{path}", harness.endpoint))
        .headers(caller_timeout::ordered_headers(harness, binding)?)
        .header("content-type", "application/json")
        .body(raw)
        .send()
        .await?)
}

async fn no_work(harness: &OrderedHarness) -> Result<()> {
    ensure!(harness.observed()?.is_empty());
    let control = harness.control(true, HttpVersion::Http1Only)?;
    let page = control
        .executions(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"ordered-correlation".try_into()?,
            None,
            100,
        )
        .await?;
    ensure!(page.execution_ids.is_empty());
    let manifest = control
        .candidate_manifest(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &harness.revision.revision_id,
            &harness.route,
            GenerationProtocol::ChatCompletions,
        )
        .await?;
    for candidate in manifest
        .runtime_observation
        .context("observation")?
        .candidates
    {
        ensure!(candidate.rate.context("rate")?.shared_count == 0);
    }
    Ok(())
}

async fn profiled() -> Result<OrderedHarness> {
    let profile: AdmissionProfileV1 = serde_json::from_value(
        json!({"schema_version":1,"rate":{"rate_group_id":"envelope-rpm","member_rpm":100},"health":{"rules":"availability_v1","failure_threshold":1,"transient_cooldown_seconds":60,"persistent_cooldown_seconds":120,"probe_lease_seconds":5},"fallback":"ordered_terminal_rejections_v1"}),
    )?;
    OrderedHarness::with_profiles(1, 8, None, Value::Null, Some(&[profile])).await
}

#[tokio::test]
async fn envelope_invalid_binding_and_raw_decoding_never_charge_arm_or_send() -> Result<()> {
    let harness = profiled().await?;
    for protocol in [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Messages,
        GenerationProtocol::Responses,
    ] {
        let original = harness.binding(vec![0], protocol).await?;
        let defaults = defaults(json!({"vendor_knob":{"k":1}}))?;
        let bound = original
            .clone()
            .with_generation_defaults_envelope(&defaults)?;
        let (path, body) = request(protocol)?;
        let raw = body.to_string();
        let payload = serde_json::to_string(&defaults)?;
        let valid = format!("{{\"{ENVELOPE_BODY_MEMBER}\":{payload},{}", &raw[1..]);
        let canonical_hash = original
            .clone()
            .with_generation_defaults(defaults.canonical.as_ref().context("canonical")?)?;
        for (binding, body) in [
            (&bound, raw),
            (&original, valid.clone()),
            (&canonical_hash, valid.clone()),
            (&bound, valid.replace("\"k\":1", "\"k\":2")),
            (&bound, valid.replace("\"k\":1", "\"k\":1,\"\\u006b\":1")),
            (
                &bound,
                valid.replace(
                    "\"schema_version\":1",
                    "\"schema_version\":1.00000000000000001",
                ),
            ),
            (
                &bound,
                format!("{{\"{ENVELOPE_BODY_MEMBER}\":{payload},{}", &valid[1..]),
            ),
            (
                &bound,
                format!(
                    "{{\"{BODY_MEMBER}\":{{\"schema_version\":1}},{}",
                    &valid[1..]
                ),
            ),
            (
                &bound,
                format!(
                    "{{\"{ENVELOPE_BODY_MEMBER}\":null,{}",
                    &body.to_string()[1..]
                ),
            ),
        ] {
            let response = send(&harness, binding, path, body).await?;
            ensure!(response.status() == StatusCode::BAD_REQUEST);
            ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
        }
    }
    no_work(&harness).await?;
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn envelope_all_nonnull_continuation_values_refuse_without_charge_or_arm() -> Result<()> {
    let harness = profiled().await?;
    for protocol in [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Messages,
        GenerationProtocol::Responses,
    ] {
        for key in [
            "previous_response_id",
            "conversation",
            "context_management",
            "cachedContent",
        ] {
            for value in [
                json!(false),
                json!(0),
                json!(""),
                json!({}),
                json!([]),
                json!("native-id"),
            ] {
                let defaults = defaults(json!({key:value}))?;
                let binding = harness
                    .binding(vec![0], protocol)
                    .await?
                    .with_generation_defaults_envelope(&defaults)?;
                let (path, mut body) = request(protocol)?;
                body[ENVELOPE_BODY_MEMBER] = serde_json::to_value(defaults)?;
                let response = send(&harness, &binding, path, body.to_string()).await?;
                ensure!(response.status() == StatusCode::CONFLICT);
                ensure!(!response.headers().contains_key("x-bitrouter-request-id"));
                let error: bitrouter_ai::tenancy::operation::Error = response.json().await?;
                ensure!(error.code == ErrorCode::CapabilityMismatch);
            }
        }
    }
    no_work(&harness).await?;
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn envelope_null_continuation_and_opaque_caller_null_keep_existing_guard_semantics()
-> Result<()> {
    let harness = harness("chat_completions", Value::Null).await?;
    for protocol in [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Messages,
        GenerationProtocol::Responses,
    ] {
        let defaults = defaults(
            json!({"previous_response_id":null,"conversation":null,"context_management":null,"cachedContent":null}),
        )?;
        let binding = harness
            .binding(vec![0], protocol)
            .await?
            .with_generation_defaults_envelope(&defaults)?;
        let id = invoke(
            &harness,
            &binding,
            defaults,
            protocol,
            false,
            HttpVersion::Http1Only,
            json!({}),
        )
        .await?;
        completed(&harness, &id, &binding).await?;
    }
    // Chat does not own a typed previous_response_id field: its explicit null
    // blocks the supplemental default globally before the existing guard.
    let defaults = defaults(json!({"previous_response_id":"native-id"}))?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?
        .with_generation_defaults_envelope(&defaults)?;
    let id = invoke(
        &harness,
        &binding,
        defaults,
        GenerationProtocol::ChatCompletions,
        false,
        HttpVersion::Http1Only,
        json!({"previous_response_id":null}),
    )
    .await?;
    completed(&harness, &id, &binding).await?;
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn envelope_store_true_messages_refuses_before_provider_send() -> Result<()> {
    let harness = harness("messages", Value::Null).await?;
    for protocol in [
        GenerationProtocol::ChatCompletions,
        GenerationProtocol::Messages,
        GenerationProtocol::Responses,
    ] {
        let defaults = defaults(json!({"store":true}))?;
        let binding = harness
            .binding(vec![0], protocol)
            .await?
            .with_generation_defaults_envelope(&defaults)?;
        let (path, mut body) = request(protocol)?;
        body[ENVELOPE_BODY_MEMBER] = serde_json::to_value(defaults)?;
        let response = send(&harness, &binding, path, body.to_string()).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
    }
    ensure!(harness.observed()?.is_empty());
    harness.fixture.service.shutdown().await?;
    Ok(())
}
