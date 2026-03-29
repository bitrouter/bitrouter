//! Agent skills client — filesystem-backed skill registry initialization.

use std::path::PathBuf;
use std::sync::Arc;

use bitrouter_config::routing::ConfigToolRoutingTable;
use bitrouter_config::{ApiProtocol, ProviderConfig};
use bitrouter_providers::agentskills::registry::FilesystemSkillRegistry;
use warp::Filter;

type RouteFilter = warp::filters::BoxedFilter<(Box<dyn warp::Reply>,)>;

/// Warp route filters and shared registry produced by skills initialization.
pub struct AgentSkillsRoutes {
    /// The shared skill registry (also used by MCP for composite tool listing).
    pub registry: Arc<FilesystemSkillRegistry>,
    /// `GET /v1/skills` CRUD endpoint.
    pub skills_list: RouteFilter,
}

/// Builder for the filesystem-backed agent skills registry.
pub struct AgentSkillsClient {
    tool_configs: Vec<(String, bitrouter_config::ToolConfig)>,
    skills_dir: PathBuf,
}

impl AgentSkillsClient {
    pub fn new(
        providers_by_protocol: &std::collections::HashMap<
            ApiProtocol,
            Vec<(String, ProviderConfig)>,
        >,
        tool_table: &ConfigToolRoutingTable,
        skills_dir: PathBuf,
    ) -> Self {
        let tool_configs = providers_by_protocol
            .get(&ApiProtocol::Skill)
            .map(|providers| {
                providers
                    .iter()
                    .flat_map(|(name, _)| {
                        tool_table
                            .tools()
                            .iter()
                            .filter(|(_, tc)| tc.endpoints.iter().any(|ep| ep.provider == *name))
                            .map(|(tn, tc)| (tn.clone(), tc.clone()))
                            .collect::<Vec<_>>()
                    })
                    .collect()
            })
            .unwrap_or_default();

        Self {
            tool_configs,
            skills_dir,
        }
    }

    pub async fn build(self) -> AgentSkillsRoutes {
        let registry = match FilesystemSkillRegistry::from_config_and_dir(
            self.tool_configs,
            self.skills_dir.clone(),
        )
        .await
        {
            Ok(reg) => Arc::new(reg),
            Err(e) => {
                tracing::warn!("failed to initialize skills from config: {e}");
                Arc::new(
                    FilesystemSkillRegistry::from_dir(self.skills_dir)
                        .await
                        .map_err(|e2| tracing::warn!("skills registry unavailable: {e2}"))
                        .unwrap_or_default(),
                )
            }
        };

        let skills_list = bitrouter_api::router::agentskills::skills_filter(registry.clone())
            .map(|r| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();

        AgentSkillsRoutes {
            registry,
            skills_list,
        }
    }
}
