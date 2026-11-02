//! Residual values keep their legacy wire meaning at boundary values and under overlap.
use super::*;

#[tokio::test]
async fn envelope_token_alias_zero_null_string_and_both_target_spellings() -> Result<()> {
    for (wire, spellings) in [
        (
            "chat_completions",
            ["max_tokens", "max_completion_tokens"].as_slice(),
        ),
        ("messages", ["max_tokens"].as_slice()),
        ("responses", ["max_tokens"].as_slice()),
    ] {
        for spelling in spellings {
            let harness =
                OrderedHarness::with_resource_edits(1, 8, None, Value::Null, None, |resources| {
                    let route = resources
                        .iter_mut()
                        .find(|row| row["reference"]["resource_kind"] == "route")
                        .context("route")?;
                    let provider = &mut route["definition"]["routing"]["providers"]["provider-0"];
                    provider["api_protocol"] = json!([{"*":wire}]);
                    provider["models"][0]["compatibility"] =
                        json!({"chat_completions":{"token_limit_field":spelling}});
                    Ok(())
                })
                .await?;
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
                        for alias in [json!(0), Value::Null, json!("invalid-token-value")] {
                            let native = match protocol {
                                GenerationProtocol::Responses => "max_output_tokens",
                                GenerationProtocol::ChatCompletions
                                    if *spelling == "max_completion_tokens" =>
                                {
                                    "max_completion_tokens"
                                }
                                _ => "max_tokens",
                            };
                            let mut callers =
                                vec![(json!({}), 23, None), (json!({native:31}), 31, None)];
                            for name in ["max_completion_tokens", "max_output_tokens"] {
                                let typed = matches!(
                                    (protocol, name),
                                    (GenerationProtocol::ChatCompletions, "max_completion_tokens")
                                        | (GenerationProtocol::Responses, "max_output_tokens")
                                );
                                if !typed {
                                    for value in [Value::Null, json!("caller-alias")] {
                                        callers.push((json!({name:value}), 23, Some(name)));
                                    }
                                }
                            }
                            for (fields, tokens, opaque_name) in callers {
                                let values = defaults(
                                    json!({"max_completion_tokens":alias,"max_output_tokens":alias}),
                                )?;
                                let binding = harness
                                    .binding(vec![0], protocol)
                                    .await?
                                    .with_generation_defaults_envelope(&values)?;
                                let id = invoke(&harness, &binding, values, protocol, stream, version, fields.clone()).await
                                    .with_context(|| format!("alias {protocol:?}->{wire}/{spelling} {version:?} stream={stream} alias={alias} caller={fields}"))?;
                                completed(&harness, &id, &binding).await?;
                                count += 1;
                                let captured = bodies(&harness)?;
                                ensure!(captured.len() == count);
                                let body = captured.last().context("provider body")?;
                                let typed = json!(tokens);
                                let same = matches!(
                                    (protocol, wire),
                                    (GenerationProtocol::ChatCompletions, "chat_completions")
                                        | (GenerationProtocol::Messages, "messages")
                                        | (GenerationProtocol::Responses, "responses")
                                );
                                let raw = |key| {
                                    if opaque_name == Some(key) {
                                        if same { fields.get(key) } else { None }
                                    } else {
                                        Some(&alias)
                                    }
                                };
                                match wire {
                                    "chat_completions" => {
                                        ensure!(body.get(*spelling) == Some(&typed));
                                        let other = if *spelling == "max_tokens" {
                                            "max_completion_tokens"
                                        } else {
                                            "max_tokens"
                                        };
                                        ensure!(body.get(other).is_none());
                                        ensure!(
                                            body.get("max_output_tokens")
                                                == raw("max_output_tokens")
                                        );
                                    }
                                    "messages" => {
                                        ensure!(body.get("max_tokens") == Some(&typed));
                                        ensure!(
                                            body.get("max_completion_tokens")
                                                == raw("max_completion_tokens")
                                        );
                                        ensure!(
                                            body.get("max_output_tokens")
                                                == raw("max_output_tokens")
                                        );
                                    }
                                    _ => {
                                        ensure!(body.get("max_output_tokens") == Some(&typed));
                                        ensure!(
                                            body.get("max_completion_tokens")
                                                == raw("max_completion_tokens")
                                        );
                                        ensure!(body.get("max_tokens").is_none());
                                    }
                                }
                            }
                        }
                    }
                }
            }
            harness.fixture.service.shutdown().await?;
        }
    }
    Ok(())
}

#[tokio::test]
async fn envelope_special_arrays_invalid_options_members_and_empty_objects() -> Result<()> {
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
                    for (raw, params) in [
                        (
                            true,
                            json!({"store":null,"parallel_tool_calls":null,"stream_options":null}),
                        ),
                        (
                            true,
                            json!({"store":[],"parallel_tool_calls":[],"stream_options":[]}),
                        ),
                        (
                            true,
                            json!({"store":7,"parallel_tool_calls":"invalid","stream_options":"invalid"}),
                        ),
                        (
                            true,
                            json!({"store":{},"parallel_tool_calls":{},"stream_options":{"include_usage":"invalid","include_obfuscation":[],"vendor":{"x":[null]}}}),
                        ),
                        (
                            false,
                            json!({"store":false,"parallel_tool_calls":false,"stream_options":{}}),
                        ),
                        (
                            false,
                            json!({"store":false,"parallel_tool_calls":false,"stream_options":{"vendor":{"x":[null]}}}),
                        ),
                    ] {
                        let values = defaults(params.clone())?;
                        let binding = harness
                            .binding(vec![0], protocol)
                            .await?
                            .with_generation_defaults_envelope(&values)?;
                        let id = invoke(&harness, &binding, values, protocol, stream, version, json!({})).await
                            .with_context(||format!("special boundary {protocol:?}->{wire} {version:?} stream={stream} params={params}"))?;
                        completed(&harness, &id, &binding).await?;
                        count += 1;
                        let captured = bodies(&harness)?;
                        ensure!(captured.len() == count);
                        let body = captured.last().context("provider body")?;
                        if raw {
                            ensure!(body.get("store") == Some(&params["store"]));
                            ensure!(
                                body.get("parallel_tool_calls")
                                    == Some(&params["parallel_tool_calls"])
                            );
                        } else if wire == "messages" {
                            ensure!(
                                body.get("store").is_none()
                                    && body.get("parallel_tool_calls").is_none()
                            );
                            ensure!(
                                body["tool_choice"]
                                    == json!({"type":"auto","disable_parallel_tool_use":true})
                            );
                        } else {
                            ensure!(body["store"] == false && body["parallel_tool_calls"] == false);
                        }
                        let expected = if wire == "chat_completions" && stream {
                            let mut options = params["stream_options"]
                                .as_object()
                                .cloned()
                                .unwrap_or_default();
                            options.insert("include_usage".into(), json!(true));
                            Some(Value::Object(options))
                        } else if raw || wire == "chat_completions" {
                            Some(params["stream_options"].clone())
                        } else {
                            None
                        };
                        ensure!(body.get("stream_options") == expected.as_ref());
                    }
                }
            }
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}

#[tokio::test]
async fn envelope_real_overlapping_calls_keep_distinct_maps_and_receipt_digests() -> Result<()> {
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
                    let first = defaults(json!({"vendor_knob":"first","only_first":{"nested":1}}))?;
                    let second =
                        defaults(json!({"vendor_knob":"second","only_second":{"nested":2}}))?;
                    let first_binding = harness
                        .binding(vec![0], protocol)
                        .await?
                        .with_generation_defaults_envelope(&first)?;
                    let second_binding = harness
                        .binding(vec![0], protocol)
                        .await?
                        .with_generation_defaults_envelope(&second)?;
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
