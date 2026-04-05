//! Conversions between `rmcp` model types and `bitrouter-core` MCP types.
//!
//! Since neither `rmcp` types nor `bitrouter-core` types are local to this
//! crate, Rust's orphan rules prevent `From` trait impls. Instead we use
//! standalone conversion functions.

use bitrouter_core::api::mcp::types::{
    McpContent, McpGetPromptResult, McpPrompt, McpPromptArgument, McpPromptContent,
    McpPromptMessage, McpResource, McpResourceContent, McpResourceTemplate, McpRole, McpTool,
    McpToolCallResult,
};

// ── Tool ────────────────────────────────────────────────────────────

pub(crate) fn tool(t: rmcp::model::Tool) -> McpTool {
    McpTool {
        name: t.name.into_owned(),
        description: t.description.map(|d| d.into_owned()),
        input_schema: serde_json::Value::Object((*t.input_schema).clone()),
    }
}

// ── CallToolResult ──────────────────────────────────────────────────

pub(crate) fn call_tool_result(r: rmcp::model::CallToolResult) -> McpToolCallResult {
    let content = r.content.into_iter().map(|c| raw_content(c.raw)).collect();
    McpToolCallResult {
        content,
        is_error: r.is_error,
    }
}

fn raw_content(c: rmcp::model::RawContent) -> McpContent {
    match c {
        rmcp::model::RawContent::Text(t) => McpContent::Text { text: t.text },
        rmcp::model::RawContent::Image(img) => McpContent::Text {
            text: format!("[image {}: {} bytes]", img.mime_type, img.data.len()),
        },
        rmcp::model::RawContent::Audio(audio) => McpContent::Text {
            text: format!("[audio {}: {} bytes]", audio.mime_type, audio.data.len()),
        },
        rmcp::model::RawContent::Resource(res) => McpContent::Text {
            text: match res.resource {
                rmcp::model::ResourceContents::TextResourceContents { text, .. } => text,
                rmcp::model::ResourceContents::BlobResourceContents { uri, .. } => {
                    format!("[blob resource: {uri}]")
                }
            },
        },
        rmcp::model::RawContent::ResourceLink(r) => McpContent::Text {
            text: format!("[resource: {}]", r.uri),
        },
    }
}

// ── Resource ────────────────────────────────────────────────────────

pub(crate) fn resource(r: rmcp::model::Resource) -> McpResource {
    let raw = r.raw;
    McpResource {
        uri: raw.uri,
        name: raw.name,
        description: raw.description,
        mime_type: raw.mime_type,
    }
}

pub(crate) fn resource_template(t: rmcp::model::ResourceTemplate) -> McpResourceTemplate {
    let raw = t.raw;
    McpResourceTemplate {
        uri_template: raw.uri_template,
        name: raw.name,
        description: raw.description,
        mime_type: raw.mime_type,
    }
}

pub(crate) fn resource_contents(c: rmcp::model::ResourceContents) -> McpResourceContent {
    match c {
        rmcp::model::ResourceContents::TextResourceContents {
            uri,
            mime_type,
            text,
            ..
        } => McpResourceContent {
            uri,
            mime_type,
            text: Some(text),
            blob: None,
        },
        rmcp::model::ResourceContents::BlobResourceContents {
            uri,
            mime_type,
            blob,
            ..
        } => McpResourceContent {
            uri,
            mime_type,
            text: None,
            blob: Some(blob),
        },
    }
}

// ── Prompt ──────────────────────────────────────────────────────────

pub(crate) fn prompt(p: rmcp::model::Prompt) -> McpPrompt {
    McpPrompt {
        name: p.name,
        description: p.description,
        arguments: p
            .arguments
            .unwrap_or_default()
            .into_iter()
            .map(prompt_argument)
            .collect(),
    }
}

fn prompt_argument(a: rmcp::model::PromptArgument) -> McpPromptArgument {
    McpPromptArgument {
        name: a.name,
        description: a.description,
        required: a.required,
    }
}

pub(crate) fn get_prompt_result(r: rmcp::model::GetPromptResult) -> McpGetPromptResult {
    McpGetPromptResult {
        description: r.description,
        messages: r.messages.into_iter().map(prompt_message).collect(),
    }
}

fn prompt_message(m: rmcp::model::PromptMessage) -> McpPromptMessage {
    McpPromptMessage {
        role: prompt_role(m.role),
        content: prompt_content(m.content),
    }
}

fn prompt_role(r: rmcp::model::PromptMessageRole) -> McpRole {
    match r {
        rmcp::model::PromptMessageRole::User => McpRole::User,
        rmcp::model::PromptMessageRole::Assistant => McpRole::Assistant,
    }
}

fn prompt_content(c: rmcp::model::PromptMessageContent) -> McpPromptContent {
    match c {
        rmcp::model::PromptMessageContent::Text { text } => McpPromptContent::Text { text },
        rmcp::model::PromptMessageContent::Resource { resource } => McpPromptContent::Resource {
            resource: resource_contents(resource.raw.resource),
        },
        rmcp::model::PromptMessageContent::Image { .. }
        | rmcp::model::PromptMessageContent::ResourceLink { .. } => McpPromptContent::Text {
            text: "[unsupported prompt content type]".into(),
        },
    }
}

// ── Error mapping ───────────────────────────────────────────────────

use bitrouter_core::api::mcp::types::McpGatewayError;

/// Map an rmcp [`ServiceError`](rmcp::service::ServiceError) to a gateway error.
pub(crate) fn service_error(name: &str, err: rmcp::service::ServiceError) -> McpGatewayError {
    McpGatewayError::UpstreamCall {
        name: name.to_owned(),
        reason: err.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tool_conversion() {
        let json = serde_json::json!({
            "name": "search",
            "description": "Search things",
            "inputSchema": {"type": "object"}
        });
        let rmcp_tool: rmcp::model::Tool = serde_json::from_value(json).expect("valid tool JSON");
        let mcp = tool(rmcp_tool);
        assert_eq!(mcp.name, "search");
        assert_eq!(mcp.description.as_deref(), Some("Search things"));
        assert_eq!(mcp.input_schema["type"], "object");
    }

    #[test]
    fn call_tool_result_text() {
        let json = serde_json::json!({
            "content": [{"type": "text", "text": "hello"}]
        });
        let rmcp_result: rmcp::model::CallToolResult =
            serde_json::from_value(json).expect("valid result JSON");
        let mcp = call_tool_result(rmcp_result);
        assert_eq!(mcp.content.len(), 1);
        assert!(matches!(&mcp.content[0], McpContent::Text { text } if text == "hello"));
        assert!(mcp.is_error.is_none());
    }

    #[test]
    fn resource_conversion() {
        let json = serde_json::json!({
            "uri": "file:///test.txt",
            "name": "test",
            "description": "A test file",
            "mimeType": "text/plain"
        });
        let rmcp_res: rmcp::model::Resource =
            serde_json::from_value(json).expect("valid resource JSON");
        let mcp = resource(rmcp_res);
        assert_eq!(mcp.uri, "file:///test.txt");
        assert_eq!(mcp.name, "test");
    }

    #[test]
    fn prompt_conversion() {
        let json = serde_json::json!({
            "name": "summarize",
            "description": "Summarize text",
            "arguments": [{
                "name": "text",
                "description": "The text",
                "required": true
            }]
        });
        let rmcp_prompt: rmcp::model::Prompt =
            serde_json::from_value(json).expect("valid prompt JSON");
        let mcp = prompt(rmcp_prompt);
        assert_eq!(mcp.name, "summarize");
        assert_eq!(mcp.arguments.len(), 1);
        assert_eq!(mcp.arguments[0].name, "text");
        assert_eq!(mcp.arguments[0].required, Some(true));
    }
}
