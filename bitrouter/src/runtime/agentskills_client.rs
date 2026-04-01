//! Agent skills client — filesystem-backed skill registry initialization.

use std::path::PathBuf;
use std::sync::Arc;

use bitrouter_config::routing::ConfigToolRoutingTable;
use bitrouter_core::routers::registry::ToolRegistry;
use bitrouter_providers::agentskills::registry::FilesystemSkillRegistry;
use warp::Filter;

type RouteFilter = warp::filters::BoxedFilter<(Box<dyn warp::Reply>,)>;

/// Warp route filters and shared registry produced by skills initialization.
pub struct AgentSkillsRoutes {
    /// The shared skill registry (also used by MCP for composite tool listing).
    pub registry: Arc<FilesystemSkillRegistry>,
    /// `GET /v1/skills` CRUD endpoint.
    pub skills_list: RouteFilter,
    /// Whether any skill tools were found in config or on disk.
    pub has_skills: bool,
}

/// Builder for the filesystem-backed agent skills registry.
pub struct AgentSkillsClient {
    tool_configs: Vec<(String, bitrouter_config::ToolConfig)>,
    skills_dir: PathBuf,
}

impl AgentSkillsClient {
    pub fn new(tool_table: &ConfigToolRoutingTable, skills_dir: PathBuf) -> Self {
        // Select tools that have an associated skill annotation.
        let tool_configs: Vec<_> = tool_table
            .tools()
            .iter()
            .filter(|(_, tc)| tc.skill.is_some())
            .map(|(tn, tc)| (tn.clone(), tc.clone()))
            .collect();

        Self {
            tool_configs,
            skills_dir,
        }
    }

    pub async fn build(self) -> AgentSkillsRoutes {
        use bitrouter_core::routers::registry::SkillService;

        let has_skills = !self.tool_configs.is_empty();

        let registry = match FilesystemSkillRegistry::from_config_and_dir(
            self.tool_configs.clone(),
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

        // Warn for bare-name skill references missing from the registry.
        // Remote refs (github:) and local paths are handled by from_config_and_dir.
        if let Ok(listed) = registry.list().await {
            for (tool_name, tool_config) in &self.tool_configs {
                if let Some(skill_ref) = &tool_config.skill
                    && !skill_ref.starts_with("github:")
                    && !skill_ref.starts_with("./")
                    && !skill_ref.starts_with("../")
                    && !skill_ref.starts_with('/')
                    && !listed.iter().any(|e| e.name == *skill_ref)
                {
                    tracing::warn!(
                        tool = %tool_name,
                        skill = %skill_ref,
                        "tool references skill not found in registry"
                    );
                }
            }
        }

        // Re-check: filesystem scan may have found skills even if config had none.
        let has_skills = has_skills || !registry.list_tools().await.is_empty();

        let skills_list = bitrouter_api::router::agentskills::skills_filter(registry.clone())
            .map(|r| Box::new(r) as Box<dyn warp::Reply>)
            .boxed();

        AgentSkillsRoutes {
            registry,
            skills_list,
            has_skills,
        }
    }
}
