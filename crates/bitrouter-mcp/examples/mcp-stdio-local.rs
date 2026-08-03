//! Test-only stdio entrypoint: serves the BitRouter origin MCP server over
//! stdio against a `LocalBackend`. Exists so the `stdio_smoke` integration
//! tests can spawn a real process and drive lifecycle and pagination behavior.
//! Not a product binary — the shipping CLI lives in `apps/bitrouter`.

use std::sync::Arc;

use bitrouter_mcp::capabilities::skill_catalog::{SkillCatalog, SkillFile, SkillFileBody};
use bitrouter_mcp::error::ToolError;
use bitrouter_mcp::server::BitrouterMcp;
use bitrouter_sdk::mcp::skills::{GetSkillResult, ListSkillsResult, SkillEntry, SkillResource};
use rmcp::model::{
    CacheScope, ListToolsResult, PaginatedRequestParams, ResultType, ServerCapabilities,
    ServerInfo, Tool,
};
use rmcp::{ErrorData, ServerHandler, ServiceExt};

/// A fixed one-skill catalog, so the roundtrip tests exercise the SEP-2640
/// surface without needing skills installed on the machine running them.
struct FixtureCatalog;

impl FixtureCatalog {
    const SKILL_MD: &'static str = "skill://git-workflow/SKILL.md";
    const GUIDE: &'static str = "skill://git-workflow/references/GUIDE.md";

    fn entry() -> SkillEntry {
        let mut frontmatter = serde_json::Map::new();
        frontmatter.insert("name".into(), "git-workflow".into());
        frontmatter.insert(
            "description".into(),
            "Follow the team's Git conventions".into(),
        );
        SkillEntry {
            uri: Self::SKILL_MD.into(),
            frontmatter,
            resources: Some(vec![
                SkillResource {
                    uri: Self::SKILL_MD.into(),
                    digest: "sha256:aa".into(),
                },
                SkillResource {
                    uri: Self::GUIDE.into(),
                    digest: "sha256:bb".into(),
                },
            ]),
            extra: serde_json::Map::new(),
        }
    }
}

#[async_trait::async_trait]
impl SkillCatalog for FixtureCatalog {
    async fn list(&self) -> Result<ListSkillsResult, ToolError> {
        Ok(ListSkillsResult {
            skills: vec![Self::entry()],
        })
    }

    async fn get(&self, uri: &str) -> Result<GetSkillResult, ToolError> {
        if uri == Self::SKILL_MD {
            Ok(GetSkillResult {
                skill: Self::entry(),
            })
        } else {
            Err(ToolError::new(format!("no installed skill at '{uri}'")))
        }
    }

    async fn read(&self, uri: &str) -> Result<SkillFile, ToolError> {
        let body = match uri {
            Self::SKILL_MD => "# Git workflow",
            Self::GUIDE => "# Guide",
            _ => {
                return Err(ToolError::new(format!(
                    "'{uri}' is not a file of any installed skill"
                )));
            }
        };
        Ok(SkillFile {
            uri: uri.to_string(),
            mime_type: Some("text/markdown".into()),
            body: SkillFileBody::Text(body.to_string()),
        })
    }
}

async fn serve_skills() -> anyhow::Result<()> {
    let server = BitrouterMcp::builder()
        .skill_catalog(Arc::new(FixtureCatalog))
        .build();
    bitrouter_mcp::server::serve_stdio(server, None).await
}

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
        Some("skills") => return serve_skills().await,
        Some(other) => anyhow::bail!("unknown pagination scenario: {other}"),
        None => {}
    }

    let server = BitrouterMcp::builder()
        .completion_local("http://127.0.0.1:4356")
        .build();
    bitrouter_mcp::server::serve_stdio(server, None).await
}
