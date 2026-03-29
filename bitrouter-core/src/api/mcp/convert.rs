//! Conversions between MCP wire types and canonical tool types.
//!
//! Mirrors the model-side pattern where each API protocol has a `convert.rs`
//! that translates provider-specific types into protocol-neutral core types.

use crate::tools::definition::{ToolAnnotations, ToolDefinition};
use crate::tools::registry::ToolEntry;
use crate::tools::result::{ToolCallResult, ToolContent};

use super::types::{McpContent, McpTool, McpToolCallResult};

// ── McpTool → ToolDefinition ─────────────────────────────────────

impl From<McpTool> for ToolDefinition {
    fn from(mcp: McpTool) -> Self {
        let input_schema = serde_json::from_value(mcp.input_schema).ok();

        Self {
            name: mcp.name,
            description: mcp.description,
            input_schema,
            annotations: None, // TODO: populate when McpTool gains annotations field
            input_modes: Vec::new(),
            output_modes: Vec::new(),
            examples: Vec::new(),
            tags: Vec::new(),
        }
    }
}

// ── ToolDefinition → McpTool ─────────────────────────────────────

impl From<ToolDefinition> for McpTool {
    fn from(def: ToolDefinition) -> Self {
        let input_schema = def
            .input_schema
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or(serde_json::json!({"type": "object"}));

        Self {
            name: def.name,
            description: def.description,
            input_schema,
        }
    }
}

// ── ToolEntry → McpTool ─────────────────────────────────────────

impl From<ToolEntry> for McpTool {
    fn from(entry: ToolEntry) -> Self {
        let input_schema = entry
            .definition
            .input_schema
            .and_then(|s| serde_json::to_value(s).ok())
            .unwrap_or_default();
        Self {
            name: entry.id,
            description: entry.definition.description,
            input_schema,
        }
    }
}

// ── McpTool → ToolEntry ──────────────────────────────────────────

impl From<McpTool> for ToolEntry {
    fn from(t: McpTool) -> Self {
        let provider = t
            .name
            .split_once('/')
            .map(|(s, _)| s)
            .unwrap_or("unknown")
            .to_owned();
        let id = t.name.clone();
        Self {
            id,
            provider,
            definition: ToolDefinition::from(t),
        }
    }
}

// ── ToolCallResult ↔ McpToolCallResult ──────────────────────────

impl From<ToolCallResult> for McpToolCallResult {
    fn from(r: ToolCallResult) -> Self {
        let content = r
            .content
            .into_iter()
            .map(|c| match c {
                ToolContent::Text { text } => McpContent::Text { text },
                ToolContent::Json { data } => McpContent::Text {
                    text: serde_json::to_string(&data).unwrap_or_default(),
                },
                ToolContent::Image { data, mime_type } => McpContent::Text {
                    text: format!("[image {mime_type}: {len} bytes]", len = data.len()),
                },
                ToolContent::Resource { uri, text } => McpContent::Text {
                    text: text.unwrap_or(uri),
                },
            })
            .collect();
        Self {
            content,
            is_error: if r.is_error { Some(true) } else { None },
        }
    }
}

impl From<McpToolCallResult> for ToolCallResult {
    fn from(r: McpToolCallResult) -> Self {
        let content = r
            .content
            .into_iter()
            .map(|c| match c {
                McpContent::Text { text } => ToolContent::Text { text },
            })
            .collect();
        Self {
            content,
            is_error: r.is_error.unwrap_or(false),
            metadata: None,
        }
    }
}

/// Convert MCP-style `Option<Map>` arguments to `serde_json::Value`.
pub fn args_to_value(
    args: Option<serde_json::Map<String, serde_json::Value>>,
) -> serde_json::Value {
    match args {
        Some(map) => serde_json::Value::Object(map),
        None => serde_json::Value::Null,
    }
}

/// Convert `serde_json::Value` to MCP-style `Option<Map>` arguments.
pub fn value_to_args(v: serde_json::Value) -> Option<serde_json::Map<String, serde_json::Value>> {
    match v {
        serde_json::Value::Object(map) => Some(map),
        serde_json::Value::Null => None,
        other => {
            let mut map = serde_json::Map::new();
            map.insert("value".to_owned(), other);
            Some(map)
        }
    }
}

// ── Helpers ──────────────────────────────────────────────────────

/// Convert an [`McpTool`] into a [`ToolDefinition`] with MCP-specific
/// annotations pre-populated.
pub fn mcp_tool_to_definition(
    mcp: McpTool,
    annotations: Option<ToolAnnotations>,
) -> ToolDefinition {
    let mut def = ToolDefinition::from(mcp);
    def.annotations = annotations;
    def
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mcp_tool_round_trips_through_definition() {
        let mcp = McpTool {
            name: "search".into(),
            description: Some("Search things".into()),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": { "type": "string" }
                }
            }),
        };

        let def = ToolDefinition::from(mcp.clone());
        assert_eq!(def.name, "search");
        assert_eq!(def.description.as_deref(), Some("Search things"));
        assert!(def.input_schema.is_some());
        assert!(def.tags.is_empty());

        let back = McpTool::from(def);
        assert_eq!(back.name, "search");
        assert_eq!(back.description.as_deref(), Some("Search things"));
    }

    #[test]
    fn mcp_tool_with_annotations() {
        let mcp = McpTool {
            name: "delete".into(),
            description: None,
            input_schema: serde_json::json!({"type": "object"}),
        };

        let annotations = Some(ToolAnnotations {
            title: Some("Delete resource".into()),
            destructive_hint: Some(true),
            ..Default::default()
        });

        let def = mcp_tool_to_definition(mcp, annotations);
        let ann = def.annotations.as_ref().expect("annotations");
        assert_eq!(ann.title.as_deref(), Some("Delete resource"));
        assert_eq!(ann.destructive_hint, Some(true));
    }
}
