use crate::models::shared::types::JsonSchema;

/// Protocol-neutral tool definition.
///
/// Canonical representation of "what a tool is" — independent of how an
/// LLM sees it ([`LanguageModelTool`]) or how it gets executed
/// ([`ToolProvider`]). MCP tools and A2A skills both convert into this type.
///
/// [`LanguageModelTool`]: crate::models::language::tool::LanguageModelTool
/// [`ToolProvider`]: super::provider::ToolProvider
#[derive(Debug, Clone)]
pub struct ToolDefinition {
    /// Machine-readable tool name (e.g. `"search"`, `"create_issue"`).
    pub name: String,
    /// Human-readable description.
    pub description: Option<String>,
    /// JSON Schema for input parameters.
    ///
    /// Present for MCP tools; `None` for A2A skills (which describe
    /// capabilities in natural language rather than typed schemas).
    pub input_schema: Option<JsonSchema>,
    /// Behavioral annotations (from MCP spec).
    pub annotations: Option<ToolAnnotations>,
    /// Supported input content types (from A2A, e.g. `"text/plain"`, `"image/png"`).
    pub input_modes: Vec<String>,
    /// Supported output content types (from A2A).
    pub output_modes: Vec<String>,
    /// Example prompts or scenarios (from A2A).
    pub examples: Vec<String>,
    /// Keyword tags for discovery (from A2A).
    pub tags: Vec<String>,
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
