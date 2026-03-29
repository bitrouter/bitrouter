//! In-memory catalog entry for a discovered skill.

use std::path::PathBuf;

use bitrouter_core::routers::registry::SkillEntry;
use bitrouter_core::tools::definition::ToolDefinition;
use bitrouter_core::tools::registry::ToolEntry;

/// Internal representation of a skill in the in-memory catalog.
///
/// Built from scanning `SKILL.md` files on disk. Converts to [`SkillEntry`]
/// for the `SkillService` API and to [`ToolEntry`] for unified tool discovery.
#[derive(Debug, Clone)]
pub(crate) struct SkillCatalogEntry {
    /// Unique skill identifier (UUID string).
    pub id: String,
    /// Skill name from SKILL.md frontmatter.
    pub name: String,
    /// What the skill does and when to use it.
    pub description: String,
    /// How the skill was registered: `"filesystem"` or a remote URL.
    pub source: String,
    /// Provider names this skill depends on for paid API access.
    pub required_apis: Vec<String>,
    /// Absolute path to the SKILL.md file on disk.
    pub path: PathBuf,
    /// ISO 8601 timestamp.
    pub created_at: String,
    /// ISO 8601 timestamp.
    pub updated_at: String,
}

impl SkillCatalogEntry {
    /// Convert to a [`SkillEntry`] for the skills CRUD API.
    pub fn to_skill_entry(&self) -> SkillEntry {
        SkillEntry {
            id: self.id.clone(),
            name: self.name.clone(),
            description: self.description.clone(),
            source: self.source.clone(),
            required_apis: self.required_apis.clone(),
            created_at: self.created_at.clone(),
            updated_at: self.updated_at.clone(),
        }
    }

    /// Convert to a [`ToolEntry`] for the unified tool discovery endpoint.
    pub fn to_tool_entry(&self) -> ToolEntry {
        ToolEntry {
            id: format!("skill/{}", self.name),
            provider: "skill".to_string(),
            definition: ToolDefinition {
                name: self.name.clone(),
                description: Some(self.description.clone()),
                input_schema: None,
                annotations: None,
                input_modes: Vec::new(),
                output_modes: Vec::new(),
                examples: Vec::new(),
                tags: Vec::new(),
            },
        }
    }
}
