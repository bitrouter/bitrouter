//! A [`ToolProvider`] that invokes tools via HTTP POST to a REST API.
//!
//! Each tool is mapped to `POST {api_base}/{tool_id}` with the tool
//! arguments serialized as the JSON request body. Authentication is
//! applied via a configurable header (e.g. `Authorization: Bearer ...`
//! or `x-api-key: ...`).

use std::sync::Arc;

use bitrouter_core::errors::{BitrouterError, Result};
use bitrouter_core::tools::provider::ToolProvider;
use bitrouter_core::tools::result::{ToolCallResult, ToolContent};

/// A tool provider that dispatches tool calls as HTTP POST requests.
pub struct RestToolProvider {
    name: String,
    api_base: String,
    /// Optional auth header: `(header_name, header_value)`.
    auth_header: Option<(String, String)>,
    client: Arc<reqwest::Client>,
}

impl RestToolProvider {
    /// Create a new REST tool provider.
    ///
    /// `auth_header` is an optional `(header_name, value)` pair applied to
    /// every request (e.g. `("Authorization", "Bearer sk-...")` or
    /// `("x-api-key", "...")`).
    pub fn new(
        name: String,
        api_base: String,
        auth_header: Option<(String, String)>,
        client: Arc<reqwest::Client>,
    ) -> Self {
        // Strip trailing slash for clean URL joining.
        let api_base = api_base.trim_end_matches('/').to_owned();
        Self {
            name,
            api_base,
            auth_header,
            client,
        }
    }
}

impl ToolProvider for RestToolProvider {
    fn provider_name(&self) -> &str {
        &self.name
    }

    async fn call_tool(
        &self,
        tool_id: &str,
        arguments: serde_json::Value,
    ) -> Result<ToolCallResult> {
        let url = format!("{}/{}", self.api_base, tool_id);

        let mut request = self.client.post(&url).json(&arguments);

        if let Some((ref header_name, ref header_value)) = self.auth_header {
            request = request.header(header_name.as_str(), header_value.as_str());
        }

        let response = request
            .send()
            .await
            .map_err(|e| BitrouterError::transport(Some(&self.name), e.to_string()))?;

        let status = response.status();
        let body = response
            .text()
            .await
            .map_err(|e| BitrouterError::transport(Some(&self.name), e.to_string()))?;

        if !status.is_success() {
            return Ok(ToolCallResult {
                content: vec![ToolContent::Text {
                    text: format!("{} error ({}): {}", tool_id, status.as_u16(), body),
                }],
                is_error: true,
                metadata: Some(serde_json::json!({ "status": status.as_u16() })),
            });
        }

        // Try to parse as JSON; fall back to plain text.
        let content = match serde_json::from_str::<serde_json::Value>(&body) {
            Ok(data) => vec![ToolContent::Json { data }],
            Err(_) => vec![ToolContent::Text { text: body }],
        };

        Ok(ToolCallResult {
            content,
            is_error: false,
            metadata: None,
        })
    }
}
