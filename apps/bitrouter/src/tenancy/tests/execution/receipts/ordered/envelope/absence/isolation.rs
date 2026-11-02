use super::*;
#[tokio::test]
async fn envelope_v2_real_overlap_isolates_layer_choice_and_unextended_control() -> Result<()> {
    for wire in ["chat_completions", "messages", "responses"] {
        let harness = harness(wire, Value::Null).await?;
        let mut count = 0;
        for protocol in [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ] {
            for version in [
                HttpVersion::Http1Only,
                #[cfg(feature = "http2")]
                HttpVersion::Http2PriorKnowledge,
            ] {
                for stream in [false, true] {
                    let first = values(
                        ConfigurationDefaults::PreserveAbsence,
                        Value::Null,
                        json!({"vendor_knob":"first","only_first":{"nested":1}}),
                    )?;
                    let second = values(
                        ConfigurationDefaults::Apply,
                        Value::Null,
                        json!({"vendor_knob":"second","only_second":{"nested":2}}),
                    )?;
                    let first_binding = harness
                        .binding(vec![0], protocol)
                        .await?
                        .with_generation_defaults_envelope_v2(&first)?;
                    let second_binding = harness
                        .binding(vec![0], protocol)
                        .await?
                        .with_generation_defaults_envelope_v2(&second)?;
                    ensure!(
                        first_binding.selection.generation_defaults_digest
                            != second_binding.selection.generation_defaults_digest
                    );
                    let gate = Arc::new(barrier::Gate {
                        entered: tokio::sync::Notify::new(),
                        release: Semaphore::new(0),
                    });
                    *harness
                        .held_provider
                        .lock()
                        .map_err(|_| anyhow::anyhow!("provider gate"))? = Some(gate.clone());
                    let first_call = invoke(
                        &harness,
                        &first_binding,
                        first,
                        protocol,
                        stream,
                        version,
                        json!({}),
                    );
                    let second_call = async {
                        tokio::time::timeout(TIMEOUT, gate.entered.notified())
                            .await
                            .context("first provider request did not enter")?;
                        // The second real HTTP call finishes while the first provider reply is held.
                        let result = invoke(
                            &harness,
                            &second_binding,
                            second,
                            protocol,
                            !stream,
                            version,
                            json!({}),
                        )
                        .await;
                        gate.release.add_permits(1);
                        result
                    };
                    let result = tokio::time::timeout(TIMEOUT, async {
                        tokio::try_join!(first_call, second_call)
                    })
                    .await;
                    gate.release.add_permits(1);
                    let (first_id, second_id) = result.context("overlapping calls timed out")??;
                    ensure!(first_id != second_id);
                    completed(&harness, &first_id, &first_binding).await?;
                    completed(&harness, &second_id, &second_binding).await?;
                    let captured = bodies(&harness)?;
                    ensure!(captured.len() == count + 2);
                    let first_body = &captured[count];
                    let second_body = &captured[count + 1];
                    ensure!(
                        first_body["vendor_knob"] == "first"
                            && first_body["only_first"] == json!({"nested":1})
                    );
                    ensure!(first_body.get("only_second").is_none());
                    ensure!(
                        second_body["vendor_knob"] == "second"
                            && second_body["only_second"] == json!({"nested":2})
                    );
                    ensure!(second_body.get("only_first").is_none());
                    matrix::assert_wire(first_body, wire, None, None)?;
                    matrix::assert_wire(second_body, wire, Some(25), Some(0.0))?;
                    count += 2;
                }
            }
        }
        let binding = harness
            .binding(vec![0], GenerationProtocol::ChatCompletions)
            .await?;
        let client = GatewayClient::with_options(
            &harness.endpoint,
            None,
            TransportOptions {
                http_version: HttpVersion::Http1Only,
                ..Default::default()
            },
        )?;
        let request: ChatRequest = serde_json::from_value(
            json!({"model":MODEL,"messages":[{"role":"user","content":"control"}]}),
        )?;
        let reply = client
            .ordered_tenant(harness.token()?, &binding)?
            .chat_reply(&request)
            .await?;
        ensure!(reply.metadata.status == 200);
        let id = reply
            .metadata
            .execution_id
            .context("control execution identity")?;
        match reply.body {
            Body::Json(body) => ensure!(serde_json::to_string(&body)?.contains("receipt reply")),
            Body::Stream(_) => anyhow::bail!("nonstreaming control returned SSE"),
        }
        completed(&harness, &id, &binding).await?;
        let captured = bodies(&harness)?;
        ensure!(captured.len() == count + 1);
        let body = captured.last().context("control body")?;
        matrix::assert_wire(body, wire, Some(25), Some(0.0))?;
        for key in [
            "vendor_knob",
            "only_first",
            "only_second",
            ENVELOPE_BODY_MEMBER,
        ] {
            ensure!(body.get(key).is_none());
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn envelope_v2_fallback_preserves_per_hop_absence_and_unknown_never_resends() -> Result<()> {
    let harness = OrderedHarness::with_resource_edits(2, 8, None, Value::Null, None, |resources| {
        let route = resources
            .iter_mut()
            .find(|row| row["reference"]["resource_kind"] == "route")
            .context("route")?;
        route["definition"]["routing"]["providers"]["provider-1"]["api_protocol"] =
            json!([{"*":"messages"}]);
        Ok(())
    })
    .await?;
    let values = values(
        ConfigurationDefaults::PreserveAbsence,
        Value::Null,
        json!({"vendor_knob":{"from":"preset"},"store":false,"parallel_tool_calls":false}),
    )?;
    let binding = harness
        .binding(vec![1, 0], GenerationProtocol::ChatCompletions)
        .await?
        .with_generation_defaults_envelope_v2(&values)?;
    let id = invoke(
        &harness,
        &binding,
        values.clone(),
        GenerationProtocol::ChatCompletions,
        false,
        HttpVersion::Http1Only,
        json!({}),
    )
    .await?;
    let receipt = harness.receipt(&id).await?;
    ensure!(receipt.outcome == Outcome::Completed);
    ensure!(
        receipt
            .attempts
            .iter()
            .map(|attempt| attempt.candidate_index)
            .collect::<Vec<_>>()
            == [1, 0]
    );
    ensure!(receipt.accepted_selection.generation_defaults_digest == Some(values.digest()?));
    let observed = bodies(&harness)?;
    ensure!(observed.len() == 2);
    matrix::assert_wire(&observed[0], "messages", None, None)?;
    matrix::assert_wire(&observed[1], "chat_completions", None, None)?;
    for body in &observed {
        ensure!(body["vendor_knob"] == json!({"from":"preset"}));
    }
    ensure!(
        observed[0]["tool_choice"]["disable_parallel_tool_use"] == true
            && observed[1]["parallel_tool_calls"] == false
    );
    harness.fixture.service.shutdown().await?;

    // A first accepted provider response with malformed JSON is uncertain work;
    // the request-local defaults must not turn it into permission to resend.
    let harness = OrderedHarness::new(2, 8, Some(1)).await?;
    let binding = harness
        .binding(vec![1, 0], GenerationProtocol::ChatCompletions)
        .await?
        .with_generation_defaults_envelope_v2(&values)?;
    ensure!(
        invoke(
            &harness,
            &binding,
            values,
            GenerationProtocol::ChatCompletions,
            false,
            HttpVersion::Http1Only,
            json!({})
        )
        .await
        .is_err()
    );
    ensure!(harness.observed()?.len() == 1);
    let page = harness
        .control(false, HttpVersion::Http1Only)?
        .executions(
            &harness.fixture.installation,
            &harness.fixture.tenant,
            &"ordered-correlation".try_into()?,
            None,
            100,
        )
        .await?;
    ensure!(page.execution_ids.len() == 1);
    let receipt = harness.receipt(&page.execution_ids[0]).await?;
    ensure!(receipt.state == ReceiptState::Unknown && receipt.outcome == Outcome::Unknown);
    ensure!(
        receipt.accepted_selection.generation_defaults_digest
            == binding.selection.generation_defaults_digest
    );
    ensure!(receipt.attempts.len() == 1 && receipt.attempts[0].candidate_index == 1);
    harness.fixture.service.shutdown().await?;
    Ok(())
}
