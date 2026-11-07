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
async fn envelope_v2_invalid_binding_and_raw_decoding_never_charge_arm_or_send() -> Result<()> {
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
            .with_generation_defaults_envelope_v2(&defaults)?;
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
            (&bound, valid.replace("preserve_absence", "apply")),
            (&bound, valid.replace("preserve_absence", "unknown")),
            (&bound, valid.replace("\"preserve_absence\"", "null")),
            (&bound, valid.replace(",\"configuration_defaults\":\"preserve_absence\"", "")),
            (&bound, valid.replace("\"configuration_defaults\":\"preserve_absence\"", "\"configuration_defaults\":\"preserve_absence\",\"configuration_defaults\":\"preserve_absence\"")),
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
async fn envelope_v2_all_nonnull_continuation_values_refuse_without_charge_or_arm() -> Result<()> {
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
                    .with_generation_defaults_envelope_v2(&defaults)?;
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
async fn envelope_v2_null_continuation_and_opaque_caller_null_keep_existing_guard_semantics()
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
            .with_generation_defaults_envelope_v2(&defaults)?;
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
        .with_generation_defaults_envelope_v2(&defaults)?;
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
async fn envelope_v2_store_true_messages_refuses_before_provider_send() -> Result<()> {
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
            .with_generation_defaults_envelope_v2(&defaults)?;
        let (path, mut body) = request(protocol)?;
        body[ENVELOPE_BODY_MEMBER] = serde_json::to_value(defaults)?;
        let response = send(&harness, &binding, path, body.to_string()).await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
    }
    ensure!(harness.observed()?.is_empty());
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn envelope_v2_policy_lock_stays_unsupported_with_valid_binding_and_no_send() -> Result<()> {
    let harness = OrderedHarness::with_resource_edits(1,8,None,Value::Null,None, |resources| {
        let route = resources.iter_mut().find(|row| row["reference"]["resource_kind"] == "route").context("route")?;
        let mut reference = route["reference"].clone();
        reference["resource_kind"] = json!("policy");
        reference["resource_id"] = json!("locked-policy");
        route["definition"]["policy"] = reference.clone();
        route["definition"]["selector"] = json!("bitrouter/locked");
        route["definition"]["routing"]["routers"] = json!({"locked":{"selection":{"kind":"policy","policy":"selected","base_model":"provider-0:model-0"}}});
        let mut document = crate::policy_lock::PolicyLock::default();
        document.policies.insert("selected".into(), crate::policy_lock::PolicyDefinition {
            tiers: [("only".into(), bitrouter_ai::routing::PolicyModelTarget::model_only("provider-0:model-0".into()))].into(),
            default_tier:Some("only".into()), ..Default::default()
        });
        resources.push(json!({"reference":reference,"definition":{"kind":"policy","name":"selected","document":document}}));
        Ok(())
    }).await?;
    let control = harness.control(true, HttpVersion::Http1Only)?;
    let caps = control.ordered_capabilities().await?;
    for mode in [
        ConfigurationDefaults::Apply,
        ConfigurationDefaults::PreserveAbsence,
    ] {
        let values = values(mode, Value::Null, json!({}))?;
        let binding = caps
            .bind(
                harness.revision.revision_id.clone(),
                "ordered-correlation".try_into()?,
                OrderedCandidateSelectionV1 {
                    schema_version: 1,
                    admission_mode: bitrouter_ai::tenancy::admission::AdmissionMode::Legacy,
                    route: harness.route.clone(),
                    candidate_indices: vec![0],
                    generation_defaults_digest: None,
                },
            )?
            .with_generation_defaults_envelope_v2(&values)?;
        let (path, mut body) = request(GenerationProtocol::ChatCompletions)?;
        body[ENVELOPE_BODY_MEMBER] = serde_json::to_value(values)?;
        let reply = send(&harness, &binding, path, body.to_string()).await?;
        ensure!(
            reply.status() == StatusCode::CONFLICT
                && !reply.headers().contains_key("x-bitrouter-request-id")
        );
        let error: bitrouter_ai::tenancy::operation::Error = reply.json().await?;
        ensure!(error.code == ErrorCode::CapabilityMismatch);
    }
    ensure!(harness.observed()?.is_empty());
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
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn envelope_v2_preserve_absence_keeps_target_store_compatibility_refusal() -> Result<()> {
    let harness = OrderedHarness::with_resource_edits(1, 8, None, Value::Null, None, |resources| {
        let route = resources
            .iter_mut()
            .find(|row| row["reference"]["resource_kind"] == "route")
            .context("route")?;
        route["definition"]["routing"]["providers"]["provider-0"]["models"][0]["compatibility"] =
            json!({"chat_completions":{"supports_store":false}});
        Ok(())
    })
    .await?;
    let original = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?;
    for mode in [
        ConfigurationDefaults::Apply,
        ConfigurationDefaults::PreserveAbsence,
    ] {
        let values = values(mode, Value::Null, json!({"store":true}))?;
        let binding = original
            .clone()
            .with_generation_defaults_envelope_v2(&values)?;
        let (path, mut body) = request(GenerationProtocol::ChatCompletions)?;
        body[ENVELOPE_BODY_MEMBER] = serde_json::to_value(values)?;
        let reply = send(&harness, &binding, path, body.to_string()).await?;
        ensure!(reply.status() == StatusCode::BAD_REQUEST);
        ensure!(harness.observed()?.is_empty());
    }
    // A supported request still succeeds on the same target after refusals.
    let values = values(
        ConfigurationDefaults::PreserveAbsence,
        Value::Null,
        json!({"store":false}),
    )?;
    let binding = original.with_generation_defaults_envelope_v2(&values)?;
    let id = invoke(
        &harness,
        &binding,
        values,
        GenerationProtocol::ChatCompletions,
        false,
        HttpVersion::Http1Only,
        json!({}),
    )
    .await?;
    completed(&harness, &id, &binding).await?;
    let captured = bodies(&harness)?;
    matrix::assert_wire(
        captured.last().context("body")?,
        "chat_completions",
        None,
        None,
    )?;
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn envelope_v2_ingress_budget_precedes_validation_and_releases_after_refusal() -> Result<()> {
    let harness = profiled().await?;
    let values = values(
        ConfigurationDefaults::PreserveAbsence,
        Value::Null,
        json!({}),
    )?;
    let binding = harness
        .binding(vec![0], GenerationProtocol::ChatCompletions)
        .await?
        .with_generation_defaults_envelope_v2(&values)?;
    let (path, mut body) = request(GenerationProtocol::ChatCompletions)?;
    body[ENVELOPE_BODY_MEMBER] = serde_json::to_value(&values)?;
    let mut held = Vec::new();
    for _ in 0..4 {
        held.push(
            harness
                .fixture
                .service
                .admission
                .ingress(16 * 1024 * 1024)
                .map_err(|e| anyhow::anyhow!("ingress: {e:?}"))?,
        );
    }
    let snapshot = || {
        harness
            .fixture
            .service
            .admission
            .ingress_for_test()
            .map_err(|e| anyhow::anyhow!("ingress: {e:?}"))
    };
    ensure!(snapshot()? == (4, 64 * 1024 * 1024, 0, 0));
    for raw in [body.to_string(), "not-json".to_owned()] {
        let reply = send(&harness, &binding, path, raw).await?;
        ensure!(reply.status() == StatusCode::TOO_MANY_REQUESTS);
        ensure!(snapshot()? == (4, 64 * 1024 * 1024, 0, 0));
    }
    drop(held);
    ensure!(snapshot()? == (0, 0, 0, 0));
    let reply = send(&harness, &binding, path, "not-json".to_owned()).await?;
    ensure!(reply.status() == StatusCode::BAD_REQUEST && snapshot()? == (0, 0, 0, 0));
    no_work(&harness).await?;
    let id = invoke(
        &harness,
        &binding,
        values,
        GenerationProtocol::ChatCompletions,
        false,
        HttpVersion::Http1Only,
        json!({}),
    )
    .await?;
    completed(&harness, &id, &binding).await?;
    ensure!(snapshot()?.0 == 0 && snapshot()?.1 == 0);
    harness.fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn envelope_v2_retains_supported_model_facts_and_refuses_required_counting() -> Result<()> {
    for counting in [false, true] {
        let harness =
            OrderedHarness::with_resource_edits(1, 8, None, Value::Null, None, |resources| {
                let route = resources
                    .iter_mut()
                    .find(|row| row["reference"]["resource_kind"] == "route")
                    .context("route")?;
                let model =
                    &mut route["definition"]["routing"]["providers"]["provider-0"]["models"][0];
                model["token_limits"] = json!({"max_output_tokens":8192,"max_input_tokens":32768});
                model["capabilities"] = json!(["tools"]);
                if counting {
                    model["input_token_counting"] = json!("responses");
                }
                Ok(())
            })
            .await?;
        let values = values(
            ConfigurationDefaults::PreserveAbsence,
            Value::Null,
            json!({}),
        )?;
        let caps = harness
            .control(true, HttpVersion::Http1Only)?
            .ordered_capabilities()
            .await?;
        let binding = caps
            .bind(
                harness.revision.revision_id.clone(),
                "ordered-correlation".try_into()?,
                OrderedCandidateSelectionV1 {
                    schema_version: 1,
                    admission_mode: bitrouter_ai::tenancy::admission::AdmissionMode::Legacy,
                    route: harness.route.clone(),
                    candidate_indices: vec![0],
                    generation_defaults_digest: None,
                },
            )?
            .with_generation_defaults_envelope_v2(&values)?;
        if counting {
            let (path, mut body) = request(GenerationProtocol::ChatCompletions)?;
            body[ENVELOPE_BODY_MEMBER] = serde_json::to_value(values)?;
            let reply = send(&harness, &binding, path, body.to_string()).await?;
            ensure!(
                reply.status() == StatusCode::CONFLICT
                    && !reply.headers().contains_key("x-bitrouter-request-id")
            );
            let error: bitrouter_ai::tenancy::operation::Error = reply.json().await?;
            ensure!(error.code == ErrorCode::CapabilityMismatch && harness.observed()?.is_empty());
        } else {
            let id = invoke(
                &harness,
                &binding,
                values,
                GenerationProtocol::ChatCompletions,
                false,
                HttpVersion::Http1Only,
                json!({}),
            )
            .await?;
            completed(&harness, &id, &binding).await?;
            let captured = bodies(&harness)?;
            matrix::assert_wire(
                captured.last().context("body")?,
                "chat_completions",
                None,
                None,
            )?;
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}
