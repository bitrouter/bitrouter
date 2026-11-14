use anyhow::{Context, Result, ensure};

use super::*;
use crate::gateway::model::{
    protocol::{
        SseEvent, chat_completions::ChatCompletionsAdapter, inbound_adapter_for,
        messages::MessagesAdapter, responses::ResponsesAdapter,
    },
    stream::SseFrame,
};
use bitrouter_ai::types::{StreamPart, ToolCallBoundary};

mod fixtures;
mod regressions;
mod review2;
mod tool_turns;

fn request(mut value: Value, stream: bool) -> Result<Prompt> {
    value["model"] = json!("fixture-model");
    value["stream"] = json!(stream);
    Ok(GenerateContentAdapter.parse_request(value)?)
}
fn event(data: Value) -> SseEvent {
    SseEvent {
        event: None,
        data: data.to_string(),
    }
}
fn frames(values: Vec<SseFrame>) -> Result<Vec<Value>> {
    values
        .into_iter()
        .map(|frame| match frame {
            SseFrame::Event { event: None, data } => Ok(serde_json::from_str(&data)?),
            _ => anyhow::bail!("expected unnamed native JSON event"),
        })
        .collect()
}
fn targets() -> [&'static dyn OutboundAdapter; 3] {
    [&ChatCompletionsAdapter, &MessagesAdapter, &ResponsesAdapter]
}
fn call(id: &str, name: Option<&str>, arguments: &str) -> StreamPart {
    StreamPart::ToolCallDelta {
        id: id.into(),
        name: name.map(str::to_owned),
        arguments: arguments.into(),
        provider_metadata: Default::default(),
    }
}
fn end(id: &str) -> StreamPart {
    StreamPart::ToolCallEnd {
        id: id.into(),
        boundary: ToolCallBoundary::Turn { complete: true },
    }
}
fn terminal() -> StreamPart {
    StreamPart::Finish {
        reason: FinishReason::Stop,
    }
}
fn minimal() -> Value {
    json!({"contents":[{"parts":[{"text":"hello"}]}]})
}
fn reply(parts: Value) -> Value {
    json!({"candidates":[{"content":{"role":"model","parts":parts},"finishReason":"STOP"}]})
}

// UT: Historical raw and alpha.30 projections remain unchanged; corrected
// projection refuses unsupported data and preserves call identity/schema.
#[test]
fn ut_native_json_preserves_reference_projection_and_separate_raw_usage() -> Result<()> {
    let codec = GenerateContentAdapter;
    let historic = request(fixtures::rich_request(false), false)?;
    assert!(codec.render_request(&historic).is_err()); // Unknown config must not disappear.
    assert!(codec.parse_response(fixtures::json_reply()).is_err()); // executableCode is not a client call.
    let mut supported = fixtures::rich_request(false);
    supported["generationConfig"]
        .as_object_mut()
        .context("config")?
        .remove("unknownFixtureConfig");
    let prompt = request(supported, false)?;
    let mut expected = fixtures::provider_request();
    expected["contents"][1]["parts"][1]["functionCall"]["id"] = json!("earlier-call");
    expected["tools"][0]["functionDeclarations"][0]["parameters"]["additionalProperties"] =
        json!(false);
    assert_eq!(codec.render_request(&prompt)?, expected);

    let mut value = fixtures::json_reply();
    assert!(
        value["candidates"][0]["content"]["parts"]
            .as_array_mut()
            .context("parts")?
            .pop()
            .is_some()
    );
    value["candidates"][0]["finishReason"] = json!("STOP");
    value["usageMetadata"]["toolUsePromptTokenCount"] = json!(0);
    value["usageMetadata"]["totalTokenCount"] = json!(16);
    let raw_usage = value["usageMetadata"].clone();
    let result = codec.parse_response(value)?;
    let usage = result.usage.as_ref().context("provider usage missing")?;
    assert_eq!(usage.origin, UsageOrigin::ProviderReported);
    assert_eq!(usage.raw.as_deref(), Some(&raw_usage));
    let mut expected = fixtures::public_json();
    expected["responseId"] = json!("provider-private-id");
    expected["candidates"][0]["index"] = json!(7);
    expected["candidates"][0]["finishReason"] = json!("STOP");
    expected["candidates"][0]["content"]["parts"][1]["functionCall"]["id"] =
        json!("provider-call-id");
    assert_eq!(
        codec.render_response(&result, &prompt, "fixture")?,
        expected
    );
    Ok(())
}

// UT: Unsigned compatible native SSE needs an observed terminal and clean EOF;
// the unchanged historical signed-text example requires exact native custody.
#[test]
fn ut_native_sse_matches_reference_and_needs_an_observed_terminal() -> Result<()> {
    let codec = GenerateContentAdapter;
    let old = fixtures::stream_frames();
    let mut decoder = codec.stream_decoder();
    assert!(decoder.decode(&event(old[0].clone())).is_err());
    assert!(decoder.finish().is_err());
    assert_ne!(old, fixtures::public_stream());
    let mut decoder = codec.stream_decoder();
    let mut encoder = codec.stream_encoder("fixture", "fixture-model");
    let mut output = Vec::new();
    for chunk in [
        json!({"responseId":"r","candidates":[{"index":0,"content":{"parts":[{"text":"first"}]}}]}),
        json!({"responseId":"r","candidates":[{"index":0,"finishReason":"STOP"}]}),
        json!({"responseId":"r","usageMetadata":{"promptTokenCount":11,"candidatesTokenCount":3}}),
    ] {
        for part in decoder.decode(&event(chunk))? {
            ensure!(!part.is_terminal());
            output.extend(frames(encoder.encode(&part)?)?);
        }
    }
    assert!(matches!(
        decoder.pending_terminal(),
        Some(StreamPart::Finish {
            reason: FinishReason::Stop
        })
    ));
    let tail = decoder.finish()?;
    assert_eq!(tail.len(), 1);
    for part in tail {
        output.extend(frames(encoder.encode(&part)?)?);
    }
    assert!(encoder.finish()?.is_empty());
    assert_eq!(
        output[0]["candidates"][0]["content"]["parts"][0]["text"],
        "first"
    );
    assert_eq!(output[1]["usageMetadata"]["totalTokenCount"], 14);
    assert_eq!(output[2]["candidates"][0]["finishReason"], "STOP");
    let mut partial = codec.stream_decoder();
    partial.decode(&event(
        json!({"candidates":[{"content":{"parts":[{"text":"partial"}]}}]}),
    ))?;
    assert!(partial.finish().is_err());
    Ok(())
}

// UT: The translated ordinary subset retains system, functions, required tool
// choice and limits. Native cache/safety constraints have separate refusal tests.
#[test]
fn ut_ordinary_adapters_convert_compatible_prompt_and_native_result() -> Result<()> {
    let codec = GenerateContentAdapter;
    let prompt = request(
        json!({
            "systemInstruction":{"parts":[{"text":"instructions"}]},
            "contents":[{"role":"user","parts":[{"text":"question"}]}],
            "tools":[{"functionDeclarations":[{"name":"lookup","description":"look up","parameters":{"type":"object"}}]}],
            "toolConfig":{"functionCallingConfig":{"mode":"ANY","allowedFunctionNames":["lookup"]}},
            "generationConfig":{"temperature":0.0,"maxOutputTokens":17}
        }),
        false,
    )?;
    let chat = ChatCompletionsAdapter.render_request(&prompt)?;
    assert_eq!(chat["messages"][0]["content"], "instructions");
    assert_eq!(chat["messages"][1]["content"], "question");
    assert_eq!(chat["tools"][0]["function"]["name"], "lookup");
    assert_eq!(chat["tool_choice"]["function"]["name"], "lookup");
    let messages = MessagesAdapter.render_request(&prompt)?;
    assert_eq!(messages["max_tokens"], 17);
    assert_eq!(messages["tools"][0]["name"], "lookup");
    assert_eq!(messages["tool_choice"]["name"], "lookup");
    let responses = ResponsesAdapter.render_request(&prompt)?;
    assert_eq!(responses["max_output_tokens"], 17);
    assert_eq!(responses["tools"][0]["name"], "lookup");
    assert_eq!(responses["tool_choice"]["name"], "lookup");
    for body in [&chat, &messages, &responses] {
        for key in [
            "safetySettings",
            "cachedContent",
            "toolConfig",
            "generationConfig",
            GOOGLE_TOP_LEVEL_EXTRA_KEY,
        ] {
            assert!(body.get(key).is_none(), "{key} reached an ordinary target");
        }
    }
    let result = ChatCompletionsAdapter.parse_response(json!({"id":"ordinary","choices":[{"index":0,"message":{"role":"assistant","content":"ordinary answer"},"finish_reason":"stop"}],"usage":{"prompt_tokens":11,"completion_tokens":3,"total_tokens":14}}))?;
    let native = codec.render_response(&result, &prompt, "fixture")?;
    assert_eq!(
        native["candidates"][0]["content"]["parts"][0]["text"],
        "ordinary answer"
    );
    assert_eq!(native["candidates"][0]["finishReason"], "STOP");
    assert_eq!(native["usageMetadata"]["totalTokenCount"], 14);
    assert!(native.get("modelVersion").is_none());
    assert!(inbound_adapter_for(&protocol()).is_none());
    Ok(())
}

// UT: Foreign targets cannot consume provider tools or signed native history.
#[test]
fn ut_ordinary_admission_refuses_foreign_tool_and_continuity_semantics() -> Result<()> {
    let tool = request(
        json!({"contents":[{"parts":[{"text":"hello"}]}],"tools":[{"googleSearch":{}}]}),
        false,
    )?;
    for part in [
        json!({"text":"thought","thought":true,"thoughtSignature":"opaque"}),
        json!({"text":"","thoughtSignature":"opaque"}),
        json!({"text":"signed","thoughtSignature":"opaque"}),
    ] {
        let signed = request(json!({"contents":[{"role":"model","parts":[part]}]}), false)?;
        for prompt in [&tool, &signed] {
            for target in targets() {
                ensure!(matches!(
                    target.render_request(prompt),
                    Err(ModelError::Incompatible { .. })
                ));
            }
        }
        assert_eq!(
            GenerateContentAdapter.render_request(&signed)?["contents"][0]["parts"][0]["thoughtSignature"],
            "opaque"
        );
    }
    assert_eq!(
        GenerateContentAdapter.render_request(&tool)?["tools"],
        json!([{"googleSearch":{}}])
    );
    Ok(())
}

// UT: Interleaved fragments flush in first-seen order only after real source
// boundaries. A parseable object or global Finish cannot replace a call end.
#[test]
fn ut_ordinary_sse_interleaving_keeps_arguments_on_their_original_calls() -> Result<()> {
    let mut encoder = GenerateContentAdapter.stream_encoder("fixture", "fixture-model");
    for part in [
        call("a", Some("first"), r#"{"x":"#),
        call("b", Some("second"), r#"{"y":"#),
        call("a", None, "17}"),
        call("b", None, "28}"),
    ] {
        assert!(encoder.encode(&part)?.is_empty());
    }
    assert!(encoder.encode(&end("b"))?.is_empty());
    let output = frames(encoder.encode(&end("a"))?)?;
    assert_eq!(output.len(), 2);
    assert_eq!(
        output[0]["candidates"][0]["content"]["parts"][0]["functionCall"],
        json!({"id":"a","name":"first","args":{"x":17}})
    );
    assert_eq!(
        output[1]["candidates"][0]["content"]["parts"][0]["functionCall"],
        json!({"id":"b","name":"second","args":{"y":28}})
    );
    assert_eq!(
        frames(encoder.encode(&terminal())?)?[0]["candidates"][0]["finishReason"],
        "STOP"
    );
    assert!(
        encoder
            .encode(&StreamPart::TextDelta {
                text: "late".into()
            })
            .is_err()
    );
    let mut encoder = GenerateContentAdapter.stream_encoder("r", "m");
    encoder.encode(&call("a", Some("first"), "{}"))?;
    assert!(encoder.encode(&terminal()).is_err());
    assert!(encoder.finish().is_err());
    Ok(())
}

// UT: Malformed financial facts stay absent and malformed tool JSON is refused.
#[test]
fn ut_malformed_usage_and_tool_arguments_do_not_become_provider_facts() -> Result<()> {
    let codec = GenerateContentAdapter;
    assert!(request(json!({"contents":[{"parts":[{"text":"hello"}]}],"generationConfig":{"__google_top_level__":{"cachedContent":"synthetic"}}}), false).is_err());
    let prompt = request(minimal(), false)?;
    let mut body = reply(json!([{"text":"answer"}]));
    body["usageMetadata"] = json!({"promptTokenCount":4,"candidatesTokenCount":"invalid"});
    let mut result = codec.parse_response(body)?;
    assert!(result.usage.is_none());
    assert!(
        codec
            .render_response(&result, &prompt, "r")?
            .get("usageMetadata")
            .is_none()
    );
    result.content.push(Content::ToolCall {
        id: "broken".into(),
        name: "lookup".into(),
        arguments: "{".into(),
        provider_executed: false,
        dynamic: false,
        provider_metadata: Default::default(),
    });
    assert!(codec.render_response(&result, &prompt, "r").is_err());
    Ok(())
}
