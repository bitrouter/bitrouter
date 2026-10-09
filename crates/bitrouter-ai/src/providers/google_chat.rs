//! Google Chat extensions and credential-bound tool-call continuity.
//!
//! The gateway returns a replay proof alongside a Google signature. Clients must
//! retain both; a native signature alone does not prove its selected account.

use base64::Engine;
use futures::{Stream, StreamExt};

use crate::auth::CredentialAuthority;
use crate::conversion::{
    ConversionDisposition, ConversionEffect, ConversionIssue, ConversionLocation, ConversionReason,
    ConversionReport, ConversionStage,
};
use crate::error::{ModelError, Result};
use crate::target::ModelTarget;
use crate::types::{
    ApiProtocol, Content, GenerateResult, Prompt, ReasoningEffort, ResponseFormat, StreamPart,
    Tool, provider_namespace, set_provider_metadata,
};

fn supports(target: &ModelTarget) -> bool {
    target.api_protocol == ApiProtocol::ChatCompletions
        && target.compatibility.chat_completions.google_extensions
}

fn signature(content: &Content) -> Option<&str> {
    let Content::ToolCall {
        provider_metadata, ..
    } = content
    else {
        return None;
    };
    provider_namespace(provider_metadata, "google")?
        .get("thoughtSignature")?
        .as_str()
}

fn proof(content: &Content, target: &ModelTarget) -> Result<String> {
    let Content::ToolCall {
        id,
        name,
        arguments,
        ..
    } = content
    else {
        return Err(ModelError::invalid_request(
            "Google replay requires a tool call",
        ));
    };
    let signature = signature(content)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ModelError::invalid_request("Google replay requires a nonempty signature")
        })?;
    if !supports(target)
        || target.api_key.is_empty()
        || name.is_empty()
        || id.is_empty()
        || arguments.is_empty()
    {
        return Err(ModelError::invalid_request(
            "Google replay authority is unavailable",
        ));
    }
    let scope = serde_json::to_string(&[
        target.provider_name.as_str(),
        target.api_base.trim_end_matches('/'),
        target.service_id.as_str(),
        target.account_label.as_deref().unwrap_or(""),
        id.as_str(),
        name.as_str(),
        arguments.as_str(),
        signature,
    ])
    .map_err(|_| ModelError::invalid_request("Google replay scope could not be encoded"))?;
    let authority =
        CredentialAuthority::derive_scoped("google-chat-tool-replay-v1", &scope, &target.api_key);
    Ok(base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(authority.proof_bytes()))
}

fn issue(
    protocol: &ApiProtocol,
    location: ConversionLocation,
    effect: ConversionEffect,
) -> ConversionIssue {
    ConversionIssue {
        stage: ConversionStage::RequestProjection,
        protocol: protocol.into(),
        location,
        reason: if effect == ConversionEffect::ReplayAuthority {
            ConversionReason::NativeReasoningAuthorityUnproven
        } else {
            ConversionReason::InputAttributeUnclassified
        },
        effect,
        disposition: if effect == ConversionEffect::ReplayAuthority {
            ConversionDisposition::RejectReplay
        } else {
            ConversionDisposition::ExcludeTarget
        },
    }
}

// Deliberately bounded to the initial Chat migration contract. Unknown schema
// keywords stay in the source prompt and exclude this target; no cleanup occurs.
fn schema_supported(schema: &serde_json::Value) -> bool {
    let Some(fields) = schema.as_object() else {
        return false;
    };
    fields.iter().all(|(key, value)| match key.as_str() {
        "type" => value.as_str().is_some_and(|kind| {
            matches!(
                kind,
                "object" | "array" | "string" | "number" | "integer" | "boolean" | "null"
            )
        }),
        "title" | "description" => value.is_string(),
        "enum" => value.as_array().is_some_and(|values| {
            !values.is_empty()
                && values
                    .iter()
                    .all(|value| value.is_string() || value.is_number())
        }),
        "properties" => value
            .as_object()
            .is_some_and(|properties| properties.values().all(schema_supported)),
        "required" => value
            .as_array()
            .is_some_and(|values| values.iter().all(serde_json::Value::is_string)),
        "items" => schema_supported(value),
        "minimum" | "maximum" => value.is_number(),
        "minItems" | "maxItems" => value.is_u64(),
        _ => false,
    })
}

/// Assess Google extensions against the concrete provider/model/account target.
pub fn admission(prompt: &Prompt, target: &ModelTarget) -> ConversionReport {
    let mut report = ConversionReport::default();
    if supports(target) {
        for (tool, definition) in prompt.tools.iter().enumerate() {
            if let Tool::Function {
                parameters, strict, ..
            } = definition
                && (strict.is_some() || !schema_supported(parameters))
            {
                let mut refusal = issue(
                    &target.api_protocol,
                    ConversionLocation::ToolDefinition { tool },
                    ConversionEffect::Unknown,
                );
                refusal.reason = ConversionReason::ToolSchemaProjectionUnclassified;
                report.issues.push(refusal);
            }
        }
        if let Some(ResponseFormat::JsonSchema {
            name,
            description,
            strict,
            schema,
        }) = &prompt.response_format
            && (name.is_some()
                || description.is_some()
                || strict.is_some()
                || !schema_supported(schema))
        {
            report.issues.push(issue(
                &target.api_protocol,
                ConversionLocation::ResponseFormat,
                ConversionEffect::Unknown,
            ));
        }
        // Disabling parallel calls and opaque Chat extras have no demonstrated
        // Google contract. Preserve them for another target instead of ignoring.
        if prompt.params.parallel_tool_calls == Some(false)
            || matches!(
                prompt.params.reasoning_effort,
                Some(ReasoningEffort::None | ReasoningEffort::Xhigh | ReasoningEffort::Max)
            )
            || prompt.params.seed.is_some()
            || prompt.params.presence_penalty.is_some()
            || prompt.params.frequency_penalty.is_some()
            || prompt
                .params
                .extras_for_protocol(&ApiProtocol::ChatCompletions)
                .filter(|(key, _)| *key == "extra_body")
                .any(|(_, extra)| {
                    extra
                        .as_object()
                        .is_none_or(|fields| fields.keys().any(|key| key != "google"))
                })
            || prompt
                .params
                .extras_for_protocol(&ApiProtocol::ChatCompletions)
                .any(|(key, _)| key != "extra_body")
        {
            report.issues.push(issue(
                &target.api_protocol,
                ConversionLocation::GenerationOptions,
                ConversionEffect::Unknown,
            ));
        }
    }
    for (message, entry) in prompt.messages.iter().enumerate() {
        for (block, content) in entry.content.iter().enumerate() {
            let Content::ToolCall {
                provider_metadata, ..
            } = content
            else {
                continue;
            };
            let Some(google) = provider_namespace(provider_metadata, "google") else {
                continue;
            };
            if google
                .keys()
                .any(|key| !matches!(key.as_str(), "thoughtSignature" | "replayProof"))
                || (google.contains_key("replayProof") && !google.contains_key("thoughtSignature"))
            {
                report.issues.push(issue(
                    &target.api_protocol,
                    ConversionLocation::MessageContent { message, block },
                    ConversionEffect::Unknown,
                ));
                continue;
            }
            if !google.contains_key("thoughtSignature") {
                continue;
            }
            let valid = proof(content, target).ok().is_some_and(|expected| {
                google.get("replayProof").and_then(|value| value.as_str())
                    == Some(expected.as_str())
            });
            if !valid {
                report.issues.push(issue(
                    &target.api_protocol,
                    ConversionLocation::MessageContent { message, block },
                    ConversionEffect::ReplayAuthority,
                ));
            }
        }
    }
    let extra = prompt
        .params
        .extra
        .get("extra_body")
        .or_else(|| prompt.params.supplemental_extra.get("extra_body"));
    if let Some(google) = extra.and_then(|value| value.get("google")) {
        let valid = supports(target)
            && !target.api_key.is_empty()
            && google.as_object().is_some_and(|fields| {
                // Explicit cache references await resource/account-scoped proof.
                fields.keys().all(|key| key == "thinking_config")
                    && fields.get("thinking_config").is_none_or(|config| {
                        // Level/budget ranges differ by model. Use the canonical
                        // declared reasoning_effort path until raw controls have
                        // their own selected-model evidence.
                        config.as_object().is_some_and(|fields| {
                            fields
                                .iter()
                                .all(|(key, value)| key == "include_thoughts" && value.is_boolean())
                        })
                    })
            });
        if !valid {
            report.issues.push(issue(
                &target.api_protocol,
                ConversionLocation::GenerationOptions,
                ConversionEffect::Unknown,
            ));
        }
    }
    report
}

/// Verify the effective static credential before a Google-extension request runs.
pub fn validate_authenticated_request(
    request: &reqwest::Request,
    target: &ModelTarget,
) -> Result<()> {
    if !supports(target) {
        return Ok(());
    }
    let expected = format!("Bearer {}", target.api_key);
    let expected_url = format!("{}/chat/completions", target.api_base.trim_end_matches('/'));
    let body_model = request
        .body()
        .and_then(reqwest::Body::as_bytes)
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(bytes).ok())
        .and_then(|body| {
            body.get("model")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned)
        });
    let valid = request
        .headers()
        .get_all(reqwest::header::AUTHORIZATION)
        .iter()
        .count()
        == 1
        && request
            .headers()
            .get_all("x-goog-api-key")
            .iter()
            .all(|value| value.to_str().ok() == Some(target.api_key.as_str()))
        && request
            .url()
            .query_pairs()
            .filter(|(name, _)| name == "key")
            .all(|(_, value)| value == target.api_key)
        && !target.api_key.is_empty()
        && request.url().as_str() == expected_url
        && body_model.as_deref() == Some(target.service_id.as_str())
        && request
            .headers()
            .get(reqwest::header::AUTHORIZATION)
            .and_then(|value| value.to_str().ok())
            == Some(expected.as_str());
    if !valid {
        return Err(ModelError::invalid_credential(
            "Google Chat replay requires the selected endpoint, model, and static bearer credential",
        ));
    }
    Ok(())
}

fn bind_call(content: &mut Content, target: &ModelTarget) -> Result<()> {
    if signature(content).is_none() {
        return Ok(());
    }
    let proof = proof(content, target)?;
    if let Content::ToolCall {
        provider_metadata, ..
    } = content
    {
        set_provider_metadata(provider_metadata, "google", "replayProof", proof.into());
    }
    Ok(())
}

/// Bind each signed complete call to the actual selected static credential.
pub fn bind_result(mut result: GenerateResult, target: &ModelTarget) -> Result<GenerateResult> {
    if target.api_protocol != ApiProtocol::ChatCompletions {
        return Ok(result);
    }
    for content in &mut result.content {
        bind_call(content, target).map_err(|error| ModelError::InvalidResponse {
            message: error.to_string(),
            usage: result.usage.clone().map(Box::new),
        })?;
    }
    Ok(result)
}

/// Emit signature/proof metadata only once the complete streamed call is known.
/// Argument content keeps streaming; late failures never manufacture a terminal.
pub fn bind_stream<S, E>(
    source: S,
    target: ModelTarget,
) -> impl Stream<Item = std::result::Result<StreamPart, E>> + Send
where
    S: Stream<Item = std::result::Result<StreamPart, E>> + Send,
    E: From<ModelError> + Send,
{
    async_stream::stream! {
        futures::pin_mut!(source);
        let mut calls: Vec<Content> = Vec::new();
        let mut usage = None;
        while let Some(part) = source.next().await {
            let mut part = match part { Ok(part) => part, Err(error) => { yield Err(error); return; } };
            if let StreamPart::Usage { usage: reported } = &part { usage = Some(Box::new(reported.clone())); }
            if !supports(&target) {
                if target.api_protocol == ApiProtocol::ChatCompletions
                    && matches!(&part, StreamPart::ToolCallDelta { provider_metadata, .. }
                        if provider_namespace(provider_metadata, "google").is_some_and(|google| google.contains_key("thoughtSignature"))) {
                    yield Err(ModelError::InvalidResponse { message: "selected Chat target has no Google continuity contract".into(), usage }.into()); return;
                }
                yield Ok(part); continue;
            }
            if let StreamPart::ToolCallDelta { id, name, arguments, provider_metadata } = &mut part {
                let index = calls.iter().position(|call| matches!(call, Content::ToolCall { id: saved, .. } if saved == id))
                    .unwrap_or_else(|| {
                        calls.push(Content::ToolCall { id:id.clone(), name:String::new(), arguments:String::new(), provider_executed:false, dynamic:false, provider_metadata:Default::default() });
                        calls.len() - 1
                    });
                if let Some(Content::ToolCall { name:saved_name, arguments:saved_arguments, provider_metadata:saved_metadata, .. }) = calls.get_mut(index) {
                    if let Some(name) = name {
                        if !saved_name.is_empty() && saved_name != name {
                            yield Err(ModelError::InvalidResponse { message: "Google stream changed tool identity".into(), usage }.into()); return;
                        }
                        *saved_name = name.clone();
                    }
                    saved_arguments.push_str(arguments);
                    if let Some(google) = provider_metadata.get("google") {
                        if saved_metadata.get("google").is_some_and(|saved| saved != google) {
                            yield Err(ModelError::InvalidResponse { message: "Google stream changed continuity metadata".into(), usage }.into()); return;
                        }
                        saved_metadata.insert("google".into(), google.clone());
                        provider_metadata.remove("google");
                    }
                }
            }
            if matches!(part, StreamPart::Finish { .. }) {
                for mut call in calls.drain(..) {
                    if signature(&call).is_none() { continue; }
                    if let Err(error) = bind_call(&mut call, &target) { yield Err(ModelError::InvalidResponse { message: error.to_string(), usage }.into()); return; }
                    if let Content::ToolCall { id, provider_metadata, .. } = call {
                        yield Ok(StreamPart::ToolCallDelta { id, name:None, arguments:String::new(), provider_metadata });
                    }
                }
            }
            yield Ok(part);
        }
    }
}
