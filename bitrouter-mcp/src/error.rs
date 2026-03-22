/// Errors produced by the MCP gateway.
#[derive(Debug, thiserror::Error)]
pub enum McpGatewayError {
    #[error("upstream '{name}' connection failed: {reason}")]
    UpstreamConnect { name: String, reason: String },

    #[error("upstream '{name}' call failed: {reason}")]
    UpstreamCall { name: String, reason: String },

    #[error("tool not found: {name}")]
    ToolNotFound { name: String },

    #[error("invalid config: {reason}")]
    InvalidConfig { reason: String },

    #[error("upstream '{name}' closed")]
    UpstreamClosed { name: String },

    #[error("parameter '{param}' denied on tool '{tool}'")]
    ParamDenied { tool: String, param: String },

    #[error("budget exceeded for account '{account_id}'")]
    BudgetExceeded { account_id: String },

    #[error("resource not found: {uri}")]
    ResourceNotFound { uri: String },

    #[error("prompt not found: {name}")]
    PromptNotFound { name: String },

    #[error("HTTP transport error for '{name}': {reason}")]
    HttpTransport { name: String, reason: String },

    #[error("session expired for '{name}': server returned 404")]
    SessionExpired { name: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_display_messages() {
        let err = McpGatewayError::ToolNotFound { name: "x/y".into() };
        assert!(err.to_string().contains("x/y"));

        let err = McpGatewayError::InvalidConfig {
            reason: "bad".into(),
        };
        assert!(err.to_string().contains("bad"));

        let err = McpGatewayError::ParamDenied {
            tool: "delete".into(),
            param: "force".into(),
        };
        assert!(err.to_string().contains("force"));
        assert!(err.to_string().contains("delete"));
    }
}
