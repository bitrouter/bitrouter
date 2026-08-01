//! Test-only stdio entrypoint: serves the BitRouter origin MCP server over
//! stdio against a `LocalBackend`. Exists so the `stdio_smoke` integration
//! tests can spawn a real process and drive lifecycle and pagination behavior.
//! Not a product binary — the shipping CLI lives in `apps/bitrouter`.

use std::sync::Arc;

use bitrouter_mcp::server::BitrouterMcp;
use rmcp::model::{
    CacheScope, ListToolsResult, PaginatedRequestParams, ResultType, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::{ErrorData, ServerHandler, ServiceExt};

#[derive(Clone, Copy)]
enum PaginationScenario {
    Private,
    ZeroTtl,
    ShorterTtl,
}

#[derive(Clone)]
struct PaginatedServer {
    scenario: PaginationScenario,
}

impl ServerHandler for PaginatedServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build())
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let cursor = request.and_then(|params| params.cursor);
        let (name, next_cursor, ttl_ms, cache_scope) = match cursor.as_deref() {
            None => (
                "first",
                Some("next".to_string()),
                60_000,
                CacheScope::Public,
            ),
            Some("next") => match self.scenario {
                PaginationScenario::Private => ("second", None, 60_000, CacheScope::Private),
                PaginationScenario::ZeroTtl => ("second", None, 0, CacheScope::Public),
                PaginationScenario::ShorterTtl => ("second", None, 1_000, CacheScope::Public),
            },
            Some(other) => {
                return Err(ErrorData::invalid_params(
                    format!("unknown test cursor: {other}"),
                    None,
                ));
            }
        };
        Ok(ListToolsResult {
            result_type: Some(ResultType::COMPLETE),
            meta: None,
            next_cursor,
            ttl_ms: Some(ttl_ms),
            cache_scope: Some(cache_scope),
            tools: vec![Tool::new(
                name,
                "pagination fixture",
                Arc::new(Default::default()),
            )],
        })
    }
}

async fn serve_paginated(scenario: PaginationScenario) -> anyhow::Result<()> {
    let service = PaginatedServer { scenario }
        .serve(rmcp::transport::stdio())
        .await?;
    service.waiting().await?;
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let scenario = std::env::args().nth(1);
    match scenario.as_deref() {
        Some("private") => return serve_paginated(PaginationScenario::Private).await,
        Some("zero-ttl") => return serve_paginated(PaginationScenario::ZeroTtl).await,
        Some("shorter-ttl") => return serve_paginated(PaginationScenario::ShorterTtl).await,
        Some(other) => anyhow::bail!("unknown pagination scenario: {other}"),
        None => {}
    }

    let server = BitrouterMcp::builder()
        .completion_local("http://127.0.0.1:4356")
        .build();
    bitrouter_mcp::server::serve_stdio(server, None).await
}
