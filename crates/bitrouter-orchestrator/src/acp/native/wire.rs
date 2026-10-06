//! Strict version-specific schema validation at the native ACP boundary.

use agent_client_protocol::schema::{v1, v2};
use agent_client_protocol::{Client, ConnectionTo, Error};
use serde::{Serialize, de::DeserializeOwned};
use serde_json::{Value, json};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) enum Wire {
    V1,
    V2,
}

pub(super) fn invalid(message: impl Into<String>) -> Error {
    Error::invalid_params().data(message.into())
}

fn checked<T: DeserializeOwned + Serialize>(value: Value) -> Result<Value, Error> {
    let typed: T = serde_json::from_value(value).map_err(|e| invalid(e.to_string()))?;
    serde_json::to_value(typed).map_err(|e| Error::internal_error().data(e.to_string()))
}

impl Wire {
    pub(super) fn validate(self, method: &str, value: Value) -> Result<Value, Error> {
        let original = value.clone();
        let normalized = match (self, method) {
            (Self::V1, "initialize") => checked::<v1::InitializeRequest>(value),
            (Self::V2, "initialize") => checked::<v2::InitializeRequest>(value),
            (Self::V1, "session/new") => checked::<v1::NewSessionRequest>(value),
            (Self::V2, "session/new") => checked::<v2::NewSessionRequest>(value),
            (Self::V1, "session/load") => checked::<v1::LoadSessionRequest>(value),
            (Self::V1, "session/resume") => checked::<v1::ResumeSessionRequest>(value),
            (Self::V2, "session/resume") => checked::<v2::ResumeSessionRequest>(value),
            (Self::V1, "session/list") => checked::<v1::ListSessionsRequest>(value),
            (Self::V2, "session/list") => checked::<v2::ListSessionsRequest>(value),
            (Self::V1, "session/prompt") => checked::<v1::PromptRequest>(value),
            (Self::V2, "session/prompt") => checked::<v2::PromptRequest>(value),
            (Self::V1, "session/cancel") => checked::<v1::CancelNotification>(value),
            (Self::V2, "session/cancel") => checked::<v2::CancelSessionNotification>(value),
            (Self::V1, "session/close") => checked::<v1::CloseSessionRequest>(value),
            (Self::V2, "session/close") => checked::<v2::CloseSessionRequest>(value),
            (_, "_bitrouter/session/resume_queue") => Ok(value),
            _ => Err(Error::method_not_found()),
        }?;
        // The upstream schema intentionally skips invalid collection members.
        // Native admission must reject them, preserving complete input and keys.
        for field in ["mcpServers", "prompt", "additionalDirectories"] {
            if let Some(raw) = original.get(field) {
                let raw = raw
                    .as_array()
                    .ok_or_else(|| invalid(format!("{field} must be an array")))?;
                let accepted = normalized
                    .get(field)
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len);
                if raw.len() != accepted {
                    return Err(invalid(format!("invalid or unsupported {field} member")));
                }
            }
        }
        if original
            .get("_meta")
            .is_some_and(|meta| !meta.is_null() && !meta.is_object())
        {
            return Err(invalid("_meta must be an object"));
        }
        Ok(original)
    }

    pub(super) fn initialize(self) -> Result<Value, Error> {
        let info = json!({"name":"bitrouter-orchestrator", "version":env!("CARGO_PKG_VERSION")});
        let meta = json!({"bitrouter":{"version":1,"schemaVersion":"1.10.2","queueResume":"_bitrouter/session/resume_queue"}});
        match self {
            Self::V1 => checked::<v1::InitializeResponse>(json!({
                "protocolVersion":1,"agentInfo":info,"authMethods":[],"_meta":meta,
                "agentCapabilities":{"loadSession":true,"promptCapabilities":{"embeddedContext":true},
                    "mcpCapabilities":{"http":true},"sessionCapabilities":{"list":{},"resume":{},"close":{}}}
            })),
            Self::V2 => checked::<v2::InitializeResponse>(json!({
                "protocolVersion":2,"info":info,"_meta":meta,
                "capabilities":{"session":{"prompt":{"embeddedContext":{}},"mcp":{"stdio":{},"http":{}}}}
            })),
        }
    }

    pub(super) async fn update(
        self,
        cx: &ConnectionTo<Client>,
        flow: &super::flow::Flow,
        session: &str,
        update: Value,
    ) -> Result<(), Error> {
        let value = json!({"sessionId":session,"update":update});
        let bytes = serde_json::to_vec(&value)
            .map_err(|e| invalid(e.to_string()))?
            .len()
            .saturating_add(512);
        let permit = flow.reserve(bytes).await?;
        flow.enqueue(permit, || match self {
            Self::V1 => cx.send_notification(
                serde_json::from_value::<v1::SessionNotification>(value)
                    .map_err(|e| invalid(e.to_string()))?,
            ),
            Self::V2 => cx.send_notification(
                serde_json::from_value::<v2::UpdateSessionNotification>(value)
                    .map_err(|e| invalid(e.to_string()))?,
            ),
        })
    }

    pub(super) async fn permission(
        self,
        cx: &ConnectionTo<Client>,
        session: &str,
        input: &crate::turn::InputRequest,
    ) -> Result<Value, Error> {
        let tool = json!({"toolCallId":input.tool_id,"title":input.tool_name,"status":"pending","rawInput":serde_json::from_str::<Value>(&input.arguments).unwrap_or(Value::String(input.arguments.clone()))});
        let options = json!([
            {"optionId":"allow_once","name":"Allow once","kind":"allow_once"},
            {"optionId":"reject_once","name":"Reject once","kind":"reject_once"}
        ]);
        let meta = json!({"bitrouter":{"version":1,"requestId":input.request_id,"toolItemId":input.tool_id}});
        match self {
            Self::V1 => {
                let req: v1::RequestPermissionRequest = serde_json::from_value(
                    json!({"sessionId":session,"toolCall":tool,"options":options,"_meta":meta}),
                )
                .map_err(|e| invalid(e.to_string()))?;
                serde_json::to_value(cx.send_request(req).block_task().await?)
                    .map_err(|e| invalid(e.to_string()))
            }
            Self::V2 => {
                let req: v2::RequestPermissionRequest = serde_json::from_value(json!({"sessionId":session,"title":input.tool_name,"subject":{"type":"tool_call","toolCall":tool},"options":options,"_meta":meta})).map_err(|e| invalid(e.to_string()))?;
                serde_json::to_value(cx.send_request(req).block_task().await?)
                    .map_err(|e| invalid(e.to_string()))
            }
        }
    }
}
