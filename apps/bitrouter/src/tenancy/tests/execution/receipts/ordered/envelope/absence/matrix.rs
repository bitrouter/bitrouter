use super::*;

pub(super) fn assert_wire(
    body: &Value,
    wire: &str,
    tokens: Option<u32>,
    temperature: Option<f64>,
) -> Result<()> {
    for key in ["max_tokens", "max_completion_tokens", "max_output_tokens"] {
        let expected = match (wire, key) {
            ("messages", "max_tokens") => Some(json!(tokens.unwrap_or(4096))),
            ("chat_completions", "max_tokens") | ("responses", "max_output_tokens") => {
                tokens.map(|n| json!(n))
            }
            _ => None,
        };
        ensure!(body.get(key) == expected.as_ref(), "{wire} {key}: {body}");
    }
    ensure!(
        body.get("temperature") == temperature.map(|n| json!(n)).as_ref(),
        "temperature: {body}"
    );
    ensure!(body.get(ENVELOPE_BODY_MEMBER).is_none());
    Ok(())
}

#[tokio::test]
async fn envelope_v2_absence_apply_caller_canonical_h1_h2_json_sse_matrix() -> Result<()> {
    for wire in ["chat_completions", "messages", "responses"] {
        // These raw retained aliases must remain filtered even when configuration
        // filling is skipped; unrelated retained provider defaults still apply.
        let harness = harness(wire, json!({"max_tokens":999,"max_completion_tokens":777,"max_output_tokens":888,"temperature":1.9,"vendor_retained":"kept"})).await?;
        for protocol in [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ] {
            let token = if protocol == GenerationProtocol::Responses {
                "max_output_tokens"
            } else {
                "max_tokens"
            };
            for version in [
                HttpVersion::Http1Only,
                #[cfg(feature = "http2")]
                HttpVersion::Http2PriorKnowledge,
            ] {
                for stream in [false, true] {
                    for mode in [
                        ConfigurationDefaults::Apply,
                        ConfigurationDefaults::PreserveAbsence,
                    ] {
                        let fill = mode == ConfigurationDefaults::Apply;
                        for (caller, canonical, tokens, temperature) in [
                            (json!({}), Value::Null, None, None),
                            (json!({token:73}), Value::Null, Some(73), None),
                            (json!({"temperature":0.375}), Value::Null, None, Some(0.375)),
                            (
                                json!({token:0,"temperature":0.0}),
                                Value::Null,
                                Some(0),
                                Some(0.0),
                            ),
                            (
                                json!({token:null,"temperature":null}),
                                Value::Null,
                                None,
                                None,
                            ),
                            (
                                json!({}),
                                json!({"schema_version":1,"max_tokens":47}),
                                Some(47),
                                None,
                            ),
                            (
                                json!({}),
                                json!({"schema_version":1,"temperature":0.625}),
                                None,
                                Some(0.625),
                            ),
                            (
                                json!({}),
                                json!({"schema_version":1,"max_tokens":0,"temperature":0.0}),
                                Some(0),
                                Some(0.0),
                            ),
                            (
                                json!({token:73,"temperature":0.375}),
                                json!({"schema_version":1,"max_tokens":47,"temperature":0.625}),
                                Some(73),
                                Some(0.375),
                            ),
                        ] {
                            let values = values(
                                mode,
                                canonical,
                                json!({"vendor_knob":"per-call","configuration_defaults":"opaque-body"}),
                            )?;
                            let binding = harness
                                .binding(vec![0], protocol)
                                .await?
                                .with_generation_defaults_envelope_v2(&values)?;
                            let id = invoke(&harness, &binding, values, protocol, stream, version, caller.clone()).await
                                .with_context(||format!("{protocol:?}->{wire} {version:?} stream={stream} mode={mode:?} caller={caller}"))?;
                            completed(&harness, &id, &binding).await?;
                            let captured = bodies(&harness)?;
                            let body = captured.last().context("body")?;
                            assert_wire(
                                body,
                                wire,
                                tokens.or(fill.then_some(25)),
                                temperature.or(fill.then_some(0.0)),
                            )?;
                            ensure!(
                                body["vendor_retained"] == "kept"
                                    && body["vendor_knob"] == "per-call"
                            );
                            ensure!(body["configuration_defaults"] == "opaque-body");
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
async fn envelope_v2_absent_typed_tokens_do_not_promote_supplemental_aliases() -> Result<()> {
    for (wire, spelling) in [
        ("chat_completions", "max_tokens"),
        ("chat_completions", "max_completion_tokens"),
        ("messages", "max_tokens"),
        ("responses", "max_output_tokens"),
    ] {
        let harness =
            OrderedHarness::with_resource_edits(1, 8, None, Value::Null, None, |resources| {
                let route = resources
                    .iter_mut()
                    .find(|row| row["reference"]["resource_kind"] == "route")
                    .context("route")?;
                let provider = &mut route["definition"]["routing"]["providers"]["provider-0"];
                provider["api_protocol"] = json!([{"*":wire}]);
                if wire == "chat_completions" {
                    provider["models"][0]["compatibility"] =
                        json!({"chat_completions":{"token_limit_field":spelling}});
                }
                Ok(())
            })
            .await?;
        for protocol in [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ] {
            for alias in [json!(0), json!(91), Value::Null, json!("invalid")] {
                for caller in [
                    json!({}),
                    json!({if protocol == GenerationProtocol::Responses {"max_output_tokens"} else {"max_tokens"}:31}),
                ] {
                    let values = values(
                        ConfigurationDefaults::PreserveAbsence,
                        Value::Null,
                        json!({"max_completion_tokens":alias,"max_output_tokens":alias}),
                    )?;
                    let binding = harness
                        .binding(vec![0], protocol)
                        .await?
                        .with_generation_defaults_envelope_v2(&values)?;
                    let id = invoke(
                        &harness,
                        &binding,
                        values,
                        protocol,
                        false,
                        HttpVersion::Http1Only,
                        caller.clone(),
                    )
                    .await?;
                    completed(&harness, &id, &binding).await?;
                    let captured = bodies(&harness)?;
                    let body = captured.last().context("body")?;
                    let tokens =
                        (!caller.as_object().context("caller")?.is_empty()).then_some(json!(31));
                    ensure!(body.get("temperature").is_none());
                    if wire == "chat_completions" {
                        ensure!(body.get(spelling) == tokens.as_ref());
                        ensure!(
                            body.get(if spelling == "max_tokens" {
                                "max_completion_tokens"
                            } else {
                                "max_tokens"
                            })
                            .is_none()
                        );
                        ensure!(body.get("max_output_tokens") == Some(&alias));
                    } else if wire == "messages" {
                        ensure!(body["max_tokens"] == tokens.unwrap_or(json!(4096)));
                        ensure!(
                            body.get("max_completion_tokens") == Some(&alias)
                                && body.get("max_output_tokens") == Some(&alias)
                        );
                    } else {
                        ensure!(
                            body.get("max_output_tokens")
                                == Some(tokens.as_ref().unwrap_or(&alias))
                        );
                        ensure!(body.get("max_completion_tokens") == Some(&alias));
                    }
                }
            }
        }
        harness.fixture.service.shutdown().await?;
    }
    Ok(())
}
