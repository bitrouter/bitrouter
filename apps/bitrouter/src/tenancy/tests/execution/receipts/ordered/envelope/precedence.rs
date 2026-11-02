use super::*;

#[tokio::test]
async fn envelope_caller_special_and_typed_or_opaque_tool_choice_ownership_matrix() -> Result<()> {
    for wire in ["chat_completions", "messages", "responses"] {
        let harness = harness(wire, Value::Null).await?;
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
                    let options = json!({"include_usage":true,"include_obfuscation":true});
                    let mut caller = json!({"store":false,"stream_options":options});
                    if protocol == GenerationProtocol::Messages {
                        caller["tool_choice"] =
                            json!({"type":"auto","disable_parallel_tool_use":true});
                    } else {
                        caller["parallel_tool_calls"] = json!(false);
                    }
                    let values = defaults(
                        json!({"store":true,"parallel_tool_calls":true,"stream_options":{"include_usage":false,"include_obfuscation":false}}),
                    )?;
                    let binding = harness
                        .binding(vec![0], protocol)
                        .await?
                        .with_generation_defaults_envelope(&values)?;
                    let id = invoke(
                        &harness, &binding, values, protocol, stream, version, caller,
                    )
                    .await?;
                    completed(&harness, &id, &binding).await?;
                    let captured = bodies(&harness)?;
                    let body = captured.last().context("provider body")?;
                    let emitted =
                        (protocol == GenerationProtocol::Messages) == (wire == "messages");
                    ensure!(body.get("store") == emitted.then_some(&json!(false)));
                    if wire == "messages" {
                        ensure!(body.get("parallel_tool_calls").is_none());
                        ensure!(
                            body["tool_choice"]
                                == json!({"type":"auto","disable_parallel_tool_use":true})
                        );
                    } else {
                        ensure!(body["parallel_tool_calls"] == false);
                    }
                    let same = matches!(
                        (protocol, wire),
                        (GenerationProtocol::ChatCompletions, "chat_completions")
                            | (GenerationProtocol::Messages, "messages")
                            | (GenerationProtocol::Responses, "responses")
                    );
                    if same {
                        ensure!(body.get("stream_options") == Some(&options));
                    } else if wire == "chat_completions" && stream {
                        ensure!(body["stream_options"] == json!({"include_usage":true}));
                    } else {
                        ensure!(body.get("stream_options").is_none());
                    }

                    for opaque in [false, true] {
                        let choice = if opaque {
                            json!({"type":"future_choice","vendor":7})
                        } else {
                            native_choice(protocol)
                        };
                        let values = defaults(
                            json!({"tool_choice":{"type":"function","function":{"name":"preset_function"}}}),
                        )?;
                        let binding = harness
                            .binding(vec![0], protocol)
                            .await?
                            .with_generation_defaults_envelope(&values)?;
                        let id = invoke(
                            &harness,
                            &binding,
                            values,
                            protocol,
                            stream,
                            version,
                            json!({"tool_choice":choice}),
                        )
                        .await?;
                        completed(&harness, &id, &binding).await?;
                        let captured = bodies(&harness)?;
                        let body = captured.last().context("provider body")?;
                        if opaque {
                            ensure!(body.get("tool_choice") == same.then_some(&choice));
                        } else {
                            ensure!(
                                body["tool_choice"]
                                    == if wire == "messages" {
                                        json!({"type":"tool","name":"caller_function"})
                                    } else if wire == "responses" {
                                        json!({"type":"function","name":"caller_function"})
                                    } else {
                                        json!({"type":"function","function":{"name":"caller_function"}})
                                    }
                            );
                        }
                    }
                }
            }
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn envelope_fallback_keeps_complete_digest_and_terminal_response_never_resends() -> Result<()>
{
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
    let values = defaults(
        json!({"vendor_knob":{"from":"preset"},"store":false,"parallel_tool_calls":false}),
    )?;
    let binding = harness
        .binding(vec![1, 0], GenerationProtocol::ChatCompletions)
        .await?
        .with_generation_defaults_envelope(&values)?;
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
        .with_generation_defaults_envelope(&values)?;
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

fn native_choice(protocol: GenerationProtocol) -> Value {
    match protocol {
        GenerationProtocol::Messages => json!({"type":"tool","name":"caller_function"}),
        GenerationProtocol::Responses => json!({"type":"function","name":"caller_function"}),
        GenerationProtocol::ChatCompletions => {
            json!({"type":"function","function":{"name":"caller_function"}})
        }
    }
}
