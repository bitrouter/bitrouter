//! Strict native part parsing with separate wire and canonical call identities.
use std::collections::HashSet;

use super::*;

pub(super) const NATIVE_ID: &str = "nativeCallId";
pub(super) const NATIVE_RESULT_ID: &str = "nativeResultId";
pub(super) const UNBOUND_RESULT: &str = "unboundResult";
pub(super) const MAX_CALLS: usize = 64;
pub(super) const MAX_PARTS: usize = 4096;
pub(super) const MAX_BYTES: usize = 1024 * 1024;

#[derive(Default)]
pub(super) struct Parts {
    ids: HashSet<String>,
    calls: Vec<Call>,
    bytes: usize,
    count: usize,
}

struct Call {
    id: String,
    name: String,
    resolved: bool,
}
impl Parts {
    pub(super) fn decode(&mut self, values: &[Value]) -> Result<Vec<Content>> {
        // Bound discriminated-part allocation before reserving canonical values.
        self.count = self
            .count
            .checked_add(values.len())
            .filter(|count| *count <= MAX_PARTS)
            .ok_or_else(|| invalid_response("GenerateContent part count exceeds codec bound"))?;
        let mut decoded = Vec::with_capacity(values.len());
        for part in values {
            let object = part
                .as_object()
                .ok_or_else(|| invalid_response("GenerateContent part must be an object"))?;
            let payloads = [
                "text",
                "functionCall",
                "functionResponse",
                "inlineData",
                "fileData",
            ];
            if payloads
                .iter()
                .filter(|key| object.contains_key(**key))
                .count()
                != 1
                || object.keys().any(|key| {
                    !payloads.contains(&key.as_str())
                        && !matches!(key.as_str(), "thoughtSignature" | "thought")
                })
            {
                return Err(invalid_response(
                    "unsupported or ambiguous GenerateContent part",
                ));
            }
            let size = bitrouter_ai::wire::json(part, MAX_BYTES)
                .map_err(|_| invalid_response("GenerateContent part exceeds codec bound"))?
                .len();
            self.bytes = self
                .bytes
                .checked_add(size)
                .filter(|size| *size <= MAX_BYTES)
                .ok_or_else(|| invalid_response("GenerateContent parts exceed codec bound"))?;
            let mut metadata = ProviderMetadata::new();
            if let Some(signature) = object.get("thoughtSignature") {
                if !signature.is_string() || signature.as_str().is_none_or(str::is_empty) {
                    return Err(invalid_response(
                        "invalid GenerateContent thought signature",
                    ));
                }
                set_provider_metadata(
                    &mut metadata,
                    PROVIDER_ID_GOOGLE,
                    GOOGLE_THOUGHT_SIGNATURE,
                    signature.clone(),
                );
            }
            if let Some(thought) = object.get("thought")
                && !thought.is_boolean()
            {
                return Err(invalid_response("invalid GenerateContent thought flag"));
            }
            let value = if let Some(text) = object.get("text") {
                let text = text
                    .as_str()
                    .ok_or_else(|| invalid_response("GenerateContent text must be a string"))?
                    .into();
                if object.get("thought").and_then(Value::as_bool) == Some(true) {
                    Content::Reasoning {
                        text,
                        provider_metadata: metadata,
                        native: None,
                    }
                } else {
                    Content::Text {
                        text,
                        provider_metadata: metadata,
                    }
                }
            } else if let Some(call) = object.get("functionCall") {
                let call = exact_object(call, &["id", "name", "args"])?;
                let name = text(call.get("name"))?.to_owned();
                let arguments = call
                    .get("args")
                    .filter(|args| args.is_object())
                    .ok_or_else(|| {
                        invalid_response(
                            "GenerateContent function arguments must be a complete object",
                        )
                    })?;
                let native_id = call.get("id");
                let id = match native_id {
                    Some(value) => text(Some(value))?.to_owned(),
                    None => format!("gc-anonymous-{}", self.calls.len()),
                };
                if self.calls.len() >= MAX_CALLS || !self.ids.insert(id.clone()) {
                    return Err(invalid_response(
                        "duplicate or excessive GenerateContent calls",
                    ));
                }
                set_provider_metadata(
                    &mut metadata,
                    PROVIDER_ID_GOOGLE,
                    NATIVE_ID,
                    native_id.cloned().unwrap_or(Value::Null),
                );
                self.calls.push(Call {
                    id: id.clone(),
                    name: name.clone(),
                    resolved: false,
                });
                Content::ToolCall {
                    id,
                    name,
                    arguments: arguments.to_string(),
                    provider_executed: false,
                    dynamic: false,
                    provider_metadata: metadata,
                }
            } else if let Some(result) = object.get("functionResponse") {
                let result = exact_object(result, &["id", "name", "response"])?;
                let name = text(result.get("name"))?.to_owned();
                let output = result
                    .get("response")
                    .filter(|value| value.is_object())
                    .ok_or_else(|| {
                        invalid_response("GenerateContent function response must be an object")
                    })?;
                let native_id = result.get("id");
                let mut matches = self.calls.iter_mut().filter(|call| {
                    !call.resolved
                        && call.name == name
                        && native_id.is_none_or(|value| value.as_str() == Some(call.id.as_str()))
                });
                let call_id = match (matches.next(), matches.next()) {
                    (Some(call), None) => {
                        call.resolved = true;
                        call.id.clone()
                    }
                    _ => {
                        set_provider_metadata(
                            &mut metadata,
                            PROVIDER_ID_GOOGLE,
                            UNBOUND_RESULT,
                            true.into(),
                        );
                        native_id
                            .map(|value| text(Some(value)))
                            .transpose()?
                            .unwrap_or(&name)
                            .to_owned()
                    }
                };
                set_provider_metadata(
                    &mut metadata,
                    PROVIDER_ID_GOOGLE,
                    NATIVE_RESULT_ID,
                    native_id.cloned().unwrap_or(Value::Null),
                );
                Content::ToolResult {
                    call_id,
                    tool_name: Some(name),
                    output: ToolResultOutput::Json {
                        value: output.clone(),
                    },
                    dynamic: false,
                    provider_metadata: metadata,
                }
            } else {
                let (key, field, inline) = if object.contains_key("inlineData") {
                    ("inlineData", "data", true)
                } else {
                    ("fileData", "fileUri", false)
                };
                let media = exact_object(&object[key], &["mimeType", field])?;
                let media_type = text(media.get("mimeType"))?.into();
                let data = text(media.get(field))?.into();
                Content::File {
                    media_type,
                    data: if inline {
                        DataContent::Base64 { data }
                    } else {
                        DataContent::Url { url: data }
                    },
                    filename: None,
                    provider_metadata: metadata,
                }
            };
            decoded.push(value);
        }
        Ok(decoded)
    }
}
fn text(value: Option<&Value>) -> Result<&str> {
    value
        .and_then(Value::as_str)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| invalid_response("GenerateContent part requires a nonempty string"))
}
fn exact_object<'a>(
    value: &'a Value,
    fields: &[&str],
) -> Result<&'a serde_json::Map<String, Value>> {
    let object = value
        .as_object()
        .ok_or_else(|| invalid_response("GenerateContent native value must be an object"))?;
    if object.keys().any(|key| !fields.contains(&key.as_str())) {
        return Err(invalid_response("unsupported GenerateContent native field"));
    }
    Ok(object)
}
