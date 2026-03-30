use crate::models::language::tool::{LanguageModelFunctionToolInputExample, LanguageModelTool};
use crate::models::shared::types::JsonSchema;

/// Protocol-neutral tool definition.
///
/// Canonical representation of "what a tool is" — independent of how an
/// LLM sees it ([`LanguageModelTool`]) or how it gets executed
/// ([`ToolProvider`]). MCP tools and skills both convert into this type.
///
/// [`ToolProvider`]: super::provider::ToolProvider
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    /// Machine-readable tool name (e.g. `"search"`, `"create_issue"`).
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// JSON Schema for input parameters.
    ///
    /// Present for MCP tools and config-enriched REST tools; `None` for
    /// skills that describe capabilities in natural language only.
    pub input_schema: Option<JsonSchema>,
    /// Behavioral annotations (from MCP spec).
    pub annotations: Option<ToolAnnotations>,
    /// Example inputs demonstrating how to call this tool.
    pub input_examples: Vec<ToolInputExample>,
}

/// An example input for a tool, used for discovery and LLM prompting.
#[derive(Debug, Clone)]
pub struct ToolInputExample {
    pub input: serde_json::Value,
}

/// Converts a [`ToolDefinition`] into a [`LanguageModelTool::Function`] for
/// injection into LLM prompts.
impl From<ToolDefinition> for LanguageModelTool {
    fn from(def: ToolDefinition) -> Self {
        LanguageModelTool::Function {
            name: def.name,
            description: def.description,
            input_schema: def.input_schema.unwrap_or_default(),
            input_examples: def
                .input_examples
                .into_iter()
                .map(|e| LanguageModelFunctionToolInputExample { input: e.input })
                .collect(),
            strict: None,
            provider_options: None,
        }
    }
}

/// MCP tool annotations — behavioral hints about a tool.
///
/// All fields are optional hints, not guarantees. Clients should not trust
/// these from untrusted servers without additional validation.
///
/// See: MCP spec 2025-03-26 `ToolAnnotations`.
#[derive(Debug, Clone, Default)]
pub struct ToolAnnotations {
    /// Human-readable title for the tool.
    pub title: Option<String>,
    /// If `true`, the tool does not modify its environment.
    pub read_only_hint: Option<bool>,
    /// If `true`, the tool may perform destructive updates.
    /// Only meaningful when `read_only_hint` is `false`.
    pub destructive_hint: Option<bool>,
    /// If `true`, calling repeatedly with the same arguments has no
    /// additional effect. Only meaningful when `read_only_hint` is `false`.
    pub idempotent_hint: Option<bool>,
    /// If `true`, the tool may interact with external entities (open world).
    /// If `false`, the tool operates in a closed domain.
    pub open_world_hint: Option<bool>,
}
