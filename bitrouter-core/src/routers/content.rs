/// Route context extracted from a request for content-aware routing.
///
/// API handlers populate this from the parsed request body so that the routing
/// table can make content-based decisions (e.g. auto-routing by detected topic
/// or estimated complexity). Non-API callers (admin endpoints, MCP hints, tool
/// routing) should use [`RouteContext::default()`] which represents an empty
/// context and causes the routing table to skip content-based logic.
#[derive(Debug, Clone, Default)]
pub struct RouteContext {
    /// Concatenated text content from all messages (lowercased).
    pub text: String,
    /// Whether the message content contains fenced code blocks.
    pub has_code_blocks: bool,
    /// Whether the request includes tool definitions.
    pub has_tools: bool,
    /// Number of messages / turns in the conversation.
    pub turn_count: usize,
    /// Total character count of the raw message text (before lowercasing).
    pub char_count: usize,
}

impl RouteContext {
    /// Returns `true` when no message text is available.
    ///
    /// An empty context indicates a non-API call site (admin, MCP, tools) or a
    /// request where content-based routing should be skipped.
    pub fn is_empty(&self) -> bool {
        self.text.is_empty()
    }
}
