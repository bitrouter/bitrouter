//! One disclosure boundary for REST and daemon MCP inspection.

use std::path::PathBuf;

use bitrouter_mcp::actions::models::{ModelsQuery, ModelsReport};
use bitrouter_mcp::actions::route::{RouteInput, RouteQuery, RouteReport};
use bitrouter_mcp::actions::status::{StatusQuery, StatusReport};
use bitrouter_mcp::backend::CallerAuth;
use bitrouter_mcp::error::ToolError;
use bitrouter_sdk::config::ControlScope;

use crate::paths::ConfigSource;

#[derive(Clone)]
pub(super) struct ReadPorts {
    pub source: ConfigSource,
    pub socket: PathBuf,
}

impl ReadPorts {
    pub async fn status_report(&self) -> anyhow::Result<StatusReport> {
        let mut report =
            crate::actions::status::DaemonStatus::new(&self.socket, Some(self.source.clone()))
                .report()
                .await
                .map_err(|_| anyhow::anyhow!("status_unavailable"))?;
        report.socket = None;
        Ok(report)
    }

    pub async fn models_report(&self) -> anyhow::Result<ModelsReport> {
        crate::actions::models::RoutableModels::new(self.source.clone(), Some(self.socket.clone()))
            .report()
            .await
            .map_err(|_| anyhow::anyhow!("models_unavailable"))
    }

    pub async fn route_report(&self, input: RouteInput) -> anyhow::Result<RouteReport> {
        crate::actions::administration::validate_identifier(&input.model)?;
        crate::actions::route::RouteAction::new(self.source.clone(), Some(self.socket.clone()))
            .report(input)
            .await
            .map_err(|_| anyhow::anyhow!("route_unavailable"))
    }
}

#[async_trait::async_trait]
impl StatusQuery for ReadPorts {
    async fn status(&self, _: &CallerAuth) -> Result<StatusReport, ToolError> {
        self.status_report()
            .await
            .map_err(|_| ToolError::new("status_unavailable"))
    }
}

#[async_trait::async_trait]
impl ModelsQuery for ReadPorts {
    async fn list_models(&self, _: &CallerAuth) -> Result<ModelsReport, ToolError> {
        self.models_report()
            .await
            .map_err(|_| ToolError::new("models_unavailable"))
    }
}

#[async_trait::async_trait]
impl RouteQuery for ReadPorts {
    async fn route(&self, input: RouteInput) -> Result<RouteReport, ToolError> {
        self.route_report(input)
            .await
            .map_err(|_| ToolError::new("route_unavailable"))
    }
}

pub(super) struct McpAuthorization;

impl bitrouter_mcp::server::RequestAuthorizer for McpAuthorization {
    fn authorize(&self, extensions: &rmcp::model::Extensions) -> Result<(), rmcp::ErrorData> {
        let caller = extensions
            .get::<http::request::Parts>()
            .and_then(|parts| parts.extensions.get::<super::auth::ControlCaller>());
        if caller.is_some_and(|caller| caller.permits(ControlScope::Read)) {
            Ok(())
        } else {
            Err(rmcp::ErrorData::invalid_request(
                "validated control:read authority required",
                None,
            ))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_mcp::server::RequestAuthorizer;

    #[test]
    fn bearer_presence_is_not_a_scope_grant() {
        let mut request = http::Request::new(());
        request.headers_mut().insert(
            http::header::AUTHORIZATION,
            http::HeaderValue::from_static("Bearer unvalidated"),
        );
        let mut extensions = rmcp::model::Extensions::new();
        extensions.insert(request.into_parts().0);
        assert!(McpAuthorization.authorize(&extensions).is_err());
    }
}
