//! OpenTelemetry GenAI semantic-convention attribute names.
//!
//! Centralized in one file so that when the spec advances out of `Development`
//! and renames attributes, the impact is bounded to this module.
//!
//! Reference: <https://opentelemetry.io/docs/specs/semconv/gen-ai/>
//!
//! Each `pub const` mirrors the semconv attribute name verbatim. The
//! [`span_name_chat`], [`span_name_execute_tool`], and
//! [`span_name_invoke_agent`] helpers produce the operation-prefixed span
//! names mandated by the spec (e.g. `chat gpt-4o`, not just `gpt-4o`).

// ── Operation kind ─────────────────────────────────────────────────────

pub const OPERATION_NAME: &str = "gen_ai.operation.name";
pub const PROVIDER_NAME: &str = "gen_ai.provider.name";

pub const OP_CHAT: &str = "chat";
pub const OP_EXECUTE_TOOL: &str = "execute_tool";
pub const OP_INVOKE_AGENT: &str = "invoke_agent";

// ── Request / response model ───────────────────────────────────────────

pub const REQUEST_MODEL: &str = "gen_ai.request.model";
pub const RESPONSE_MODEL: &str = "gen_ai.response.model";
pub const RESPONSE_ID: &str = "gen_ai.response.id";
pub const RESPONSE_FINISH_REASONS: &str = "gen_ai.response.finish_reasons";
pub const REQUEST_STREAM: &str = "gen_ai.request.stream";
pub const RESPONSE_TIME_TO_FIRST_CHUNK: &str = "gen_ai.response.time_to_first_chunk";

// ── Token usage ────────────────────────────────────────────────────────

pub const USAGE_INPUT_TOKENS: &str = "gen_ai.usage.input_tokens";
pub const USAGE_OUTPUT_TOKENS: &str = "gen_ai.usage.output_tokens";
pub const USAGE_CACHE_CREATION_INPUT_TOKENS: &str = "gen_ai.usage.cache_creation.input_tokens";
pub const USAGE_CACHE_READ_INPUT_TOKENS: &str = "gen_ai.usage.cache_read.input_tokens";
pub const USAGE_REASONING_OUTPUT_TOKENS: &str = "gen_ai.usage.reasoning.output_tokens";

// ── Identity ───────────────────────────────────────────────────────────

pub const CONVERSATION_ID: &str = "gen_ai.conversation.id";
pub const AGENT_ID: &str = "gen_ai.agent.id";
pub const AGENT_NAME: &str = "gen_ai.agent.name";

/// `user.id` is the OTel general semconv key, but is also referenced by the
/// GenAI spec for end-user attribution.
pub const USER_ID: &str = "user.id";

/// OpenRouter compatibility: duplicate of [`CONVERSATION_ID`].
pub const SESSION_ID: &str = "session.id";

// ── Content (Opt-In tier) ──────────────────────────────────────────────

pub const INPUT_MESSAGES: &str = "gen_ai.input.messages";
pub const OUTPUT_MESSAGES: &str = "gen_ai.output.messages";
pub const SYSTEM_INSTRUCTIONS: &str = "gen_ai.system_instructions";
pub const TOOL_CALL_ARGUMENTS: &str = "gen_ai.tool.call.arguments";
pub const TOOL_CALL_RESULT: &str = "gen_ai.tool.call.result";

// ── BitRouter extensions ───────────────────────────────────────────────

pub const BR_ACCOUNT_ID: &str = "bitrouter.account_id";
pub const BR_ROUTE: &str = "bitrouter.route";
pub const BR_POLICY_ID: &str = "bitrouter.policy_id";
pub const BR_KEY_ID: &str = "bitrouter.key_id";
pub const BR_LATENCY_MS: &str = "bitrouter.latency_ms";

// ── Errors (general OTel semconv) ──────────────────────────────────────

pub const ERROR_TYPE: &str = "error.type";

// ── Span name helpers ──────────────────────────────────────────────────

pub fn span_name_chat(model: &str) -> String {
    format!("{OP_CHAT} {model}")
}

pub fn span_name_execute_tool(tool: &str) -> String {
    format!("{OP_EXECUTE_TOOL} {tool}")
}

pub fn span_name_invoke_agent(agent: &str) -> String {
    format!("{OP_INVOKE_AGENT} {agent}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn span_names_use_spec_format() {
        assert_eq!(span_name_chat("gpt-4o"), "chat gpt-4o");
        assert_eq!(span_name_execute_tool("search"), "execute_tool search");
        assert_eq!(
            span_name_invoke_agent("claude-code"),
            "invoke_agent claude-code"
        );
    }

    #[test]
    fn attribute_keys_match_genai_spec() {
        // Guard against accidental renames. These strings are the spec; they
        // travel out to every receiver. Bumping them is a wire-format change.
        assert_eq!(OPERATION_NAME, "gen_ai.operation.name");
        assert_eq!(PROVIDER_NAME, "gen_ai.provider.name");
        assert_eq!(REQUEST_MODEL, "gen_ai.request.model");
        assert_eq!(RESPONSE_MODEL, "gen_ai.response.model");
        assert_eq!(USAGE_INPUT_TOKENS, "gen_ai.usage.input_tokens");
        assert_eq!(USAGE_OUTPUT_TOKENS, "gen_ai.usage.output_tokens");
        assert_eq!(CONVERSATION_ID, "gen_ai.conversation.id");
        assert_eq!(USER_ID, "user.id");
        assert_eq!(ERROR_TYPE, "error.type");
    }
}
