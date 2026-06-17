//! The nested-completion interface: a billing-agnostic way for a server tool to
//! run one model completion through a pipeline and read its text + usage back.
//!
//! `run` receives the per-request [`ToolContext`] so a deployment that needs
//! identity or billing can read it off the context — this crate never models
//! either. The plain [`PipelineNestedRunner`] below uses only the caller carried
//! on the context; a deployment that needs metering supplies its own
//! [`NestedRunner`] (or wires a settlement recorder onto the sub-pipeline).

use std::sync::Arc;

use async_trait::async_trait;

use crate::language_model::Pipeline;
use crate::language_model::server_tools::toolset::ToolContext;
use crate::language_model::types::{
    Content, GenerationParams, Message, PipelineRequest, Prompt, ProviderMetadata, ResponseFormat,
    Role, Tool, Usage,
};

/// One nested completion.
#[derive(Clone, Debug)]
pub struct NestedRequest {
    /// Model that serves the nested call.
    pub model: String,
    /// System instructions for the nested model, if any.
    pub system: Option<String>,
    /// The user turn.
    pub user: String,
    /// Server tools the nested model may use (e.g. forwarded web_search).
    pub tools: Vec<Tool>,
    /// Optional structured-output contract (used by the Fusion judge).
    pub response_format: Option<ResponseFormat>,
}

/// A nested completion's final text, the serving model, and its usage.
#[derive(Clone, Debug, Default, PartialEq)]
pub struct NestedOutcome {
    /// The model that served the call.
    pub model: String,
    /// The concatenated text content of the response.
    pub text: String,
    /// Token usage reported by the nested completion.
    pub usage: Usage,
}

/// Runs a nested completion, returning its outcome or a human-readable error
/// (surfaced to the calling model as a tool result with `status: "error"`).
#[async_trait]
pub trait NestedRunner: Send + Sync {
    /// Run one nested completion. `ctx` is the per-request context of the
    /// server-tool call that triggered this nested run.
    async fn run(
        &self,
        request: NestedRequest,
        ctx: &ToolContext,
    ) -> std::result::Result<NestedOutcome, String>;
}

/// The plain runner: runs the nested completion through a sub-pipeline, using
/// the caller carried on the [`ToolContext`]. It carries no identity or billing
/// of its own; metering is whatever the sub-pipeline's settlement recorders do
/// (a custom [`NestedRunner`] can add more).
pub struct PipelineNestedRunner {
    sub_pipeline: Arc<Pipeline>,
}

impl PipelineNestedRunner {
    /// Build a runner over a sub-completion pipeline.
    pub fn new(sub_pipeline: Arc<Pipeline>) -> Self {
        Self { sub_pipeline }
    }
}

#[async_trait]
impl NestedRunner for PipelineNestedRunner {
    async fn run(
        &self,
        request: NestedRequest,
        ctx: &ToolContext,
    ) -> std::result::Result<NestedOutcome, String> {
        let prompt = Prompt {
            model: request.model.clone(),
            system: request.system,
            system_provider_metadata: ProviderMetadata::new(),
            messages: vec![Message::text(Role::User, request.user)],
            tools: request.tools,
            params: GenerationParams::default(),
            response_format: request.response_format,
            tool_choice: None,
            stream: false,
        };
        let req = PipelineRequest::new(request.model.clone(), ctx.caller().clone(), prompt);
        let resp = self
            .sub_pipeline
            .execute(req)
            .await
            .map_err(|e| e.to_string())?;
        let text = resp
            .result
            .content
            .iter()
            .filter_map(|c| match c {
                Content::Text { text, .. } => Some(text.as_str()),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("");
        Ok(NestedOutcome {
            model: request.model,
            text,
            usage: resp.result.usage.unwrap_or_default(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::language_model::types::ResponseFormat;

    fn assert_send_sync<T: Send + Sync>() {}

    #[test]
    fn seam_types_are_send_sync_and_carry_response_format() {
        assert_send_sync::<NestedRequest>();
        assert_send_sync::<NestedOutcome>();
        let req = NestedRequest {
            model: "test/model".into(),
            system: None,
            user: "hi".into(),
            tools: vec![],
            response_format: Some(ResponseFormat::JsonSchema {
                name: Some("analysis".into()),
                description: None,
                strict: Some(true),
                schema: serde_json::json!({ "type": "object" }),
            }),
        };
        assert!(req.response_format.is_some());
    }
}
