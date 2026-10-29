//! Application-owned projections between model values and provider wire fields.

use bitrouter_ai::types::{
    ApiProtocol, DataContent, GenerationParams, ToolResultContentPart, ToolResultOutput,
};

/// Interpret an untyped provider tool result, preserving string versus JSON shape.
pub(crate) fn parse_untyped_tool_result(value: &serde_json::Value) -> ToolResultOutput {
    match value {
        serde_json::Value::String(s) => ToolResultOutput::Text { value: s.clone() },
        other => ToolResultOutput::Json {
            value: other.clone(),
        },
    }
}

/// Project a tool result onto string-only provider wires. Media references are
/// omitted and execution denials retain the established fallback text.
pub(crate) fn render_tool_result_output(output: &ToolResultOutput) -> String {
    match output {
        ToolResultOutput::Text { value } | ToolResultOutput::ErrorText { value } => value.clone(),
        ToolResultOutput::Json { value } | ToolResultOutput::ErrorJson { value } => {
            value.to_string()
        }
        ToolResultOutput::Content { value } => value
            .iter()
            .filter_map(|p| match p {
                ToolResultContentPart::Text { text } => Some(text.as_str()),
                // Media bytes and provider file references have no string form
                // on a string-only tool wire; both drop out of the collapse.
                ToolResultContentPart::Media { .. } | ToolResultContentPart::FileId { .. } => None,
            })
            .collect(),
        // A denied execution collapses to its reason, or the AI SDK's default
        // denial sentinel when none was given.
        // <https://github.com/vercel/ai/blob/8e650ab809ac47de5d16f26bf544a9a73b0d39a3/packages/openai/src/responses/convert-to-openai-responses-input.ts>
        ToolResultOutput::ExecutionDenied { reason } => reason
            .clone()
            .unwrap_or_else(|| "Tool call execution denied.".to_string()),
    }
}

/// Render the URL field used by the existing Chat and Responses media codecs.
pub(crate) fn data_to_url(data: &DataContent, media_type: &str) -> String {
    match data {
        DataContent::Base64 { data } => format!("data:{media_type};base64,{data}"),
        DataContent::Url { url } => url.clone(),
    }
}

/// Decode the existing URL-or-data-URI representation without changing its
/// historical fallback to a URL for other inputs.
pub(crate) fn data_from_url(value: &str) -> (Option<String>, DataContent) {
    if let Some(rest) = value.strip_prefix("data:")
        && let Some((meta, payload)) = rest.split_once(',')
        && meta.contains(";base64")
    {
        let media_type = meta
            .split(';')
            .next()
            .filter(|m| !m.is_empty())
            .map(str::to_string);
        return (
            media_type,
            DataContent::Base64 {
                data: payload.to_string(),
            },
        );
    }
    (
        None,
        DataContent::Url {
            url: value.to_string(),
        },
    )
}

/// Fill a missing provider citation identity with the established URL/index key.
pub(crate) fn synthesize_source_id(url: &str, index: usize) -> String {
    format!("{url}#{index}")
}

/// Select source-owned extras only for their original protocol, followed by
/// server defaults. Preserve iteration order for the existing renderers.
pub(crate) fn extras_for_protocol<'a>(
    params: &'a GenerationParams,
    protocol: &'a ApiProtocol,
) -> impl Iterator<Item = (&'a String, &'a serde_json::Value)> + 'a {
    let source_matches = params
        .extra_protocol
        .as_ref()
        .is_none_or(|source| source == protocol);
    params
        .extra
        .iter()
        .filter(move |_| source_matches)
        .chain(params.supplemental_extra.iter())
}

/// Read one outbound field, preferring source-wire data over server defaults.
pub(crate) fn extra_value_for_protocol<'a>(
    params: &'a GenerationParams,
    protocol: &ApiProtocol,
    key: &str,
) -> Option<&'a serde_json::Value> {
    let source_value = params
        .extra_protocol
        .as_ref()
        .is_none_or(|source| source == protocol)
        .then(|| params.extra.get(key))
        .flatten();
    source_value.or_else(|| params.supplemental_extra.get(key))
}
