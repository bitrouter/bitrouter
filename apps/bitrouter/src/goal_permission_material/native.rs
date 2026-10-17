//! Original native input metadata; no ACP request or session is synthesized.
use super::*;
use bitrouter_orchestrator::goal::workflow::ThreadExecution;
use bitrouter_orchestrator::turn::InputRequest;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Request {
    pub native: ThreadExecution,
    pub input: InputRequest,
}

impl Request {
    pub(crate) fn decode(bytes: &[u8]) -> Result<Self> {
        anyhow::ensure!(
            bytes.len() <= bitrouter_orchestrator::goal::MAX_RECORD_BYTES,
            "native permission exceeds its material bound"
        );
        let value: Self = serde_json::from_slice(bytes)?;
        for id in [
            &value.native.thread_id,
            &value.native.turn_id,
            &value.input.request_id,
            &value.input.tool_id,
        ] {
            anyhow::ensure!(
                !id.is_empty() && id.len() <= 128 && !id.chars().any(char::is_control),
                "invalid native permission identity"
            );
        }
        anyhow::ensure!(
            !value.input.tool_name.is_empty()
                && value.input.tool_name.len() <= 256
                && !value.input.tool_name.chars().any(char::is_control),
            "invalid native tool name"
        );
        Ok(value)
    }
}

/// Scope comes from the built-in tool contract, never an arbitrary MCP name.
pub(crate) fn tool_scope(name: &str, arguments: &str) -> Option<Value> {
    let kind = match name {
        "read" => "read",
        "write" | "edit" => "edit",
        "glob" | "grep" => "search",
        _ => return None,
    };
    let mut arguments = serde_json::from_str::<Value>(arguments).ok()?;
    let fields = arguments.as_object_mut()?;
    // The native search tools use the workspace root when path is omitted.
    if matches!(name, "glob" | "grep") && fields.get("path").is_none_or(Value::is_null) {
        fields.insert("path".into(), Value::String(".".into()));
    }
    Some(serde_json::json!({"kind":kind,"rawInput":arguments}))
}
