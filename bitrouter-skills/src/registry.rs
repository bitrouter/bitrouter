//! Skill registries — config-driven and DB-backed.
//!
//! Both registries implement [`ToolRegistry`] from bitrouter-core so skills
//! appear alongside MCP tools in unified discovery endpoints.

use std::collections::HashMap;
use std::sync::Arc;

use chrono::Utc;
use sea_orm::{
    ActiveModelTrait, ColumnTrait, DatabaseConnection, EntityTrait, QueryFilter, Set,
};
use uuid::Uuid;

use bitrouter_core::routers::registry::{SkillEntry, SkillService, ToolEntry, ToolRegistry};

use crate::entity::skill::{self, ActiveModel, Entity as SkillEntity};
use crate::skill::{InstalledBy, Skill, SkillSource};

// ── Config-driven registry (no DB) ────────────────────────────────

/// Immutable skill registry loaded from configuration.
///
/// Parallel to `ConfigAgentRegistry` in `bitrouter-config`. The caller
/// (typically the binary) converts `SkillConfig` entries into [`ToolEntry`]
/// values so this crate stays independent of `bitrouter-config`.
pub struct ConfigSkillRegistry {
    entries: Vec<ToolEntry>,
}

impl ConfigSkillRegistry {
    /// Build from pre-converted tool entries.
    ///
    /// The caller is responsible for converting `SkillConfig` into `ToolEntry`
    /// (typically: `id = "skill/{name}"`, `provider = "skill"`,
    /// `input_schema = None`).
    pub fn new(entries: Vec<ToolEntry>) -> Self {
        Self { entries }
    }
}

impl ToolRegistry for ConfigSkillRegistry {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        self.entries.clone()
    }
}

// ── DB-backed registry ─────────────────────────────────────────────

/// Persistent skill registry backed by sea-orm.
///
/// Supports CRUD operations and implements [`ToolRegistry`] for discovery.
pub struct SkillRegistry {
    db: Arc<DatabaseConnection>,
}

impl SkillRegistry {
    pub fn new(db: Arc<DatabaseConnection>) -> Self {
        Self { db }
    }

    /// Register a new skill (or update if the name already exists).
    pub async fn register(&self, skill: Skill) -> Result<Uuid, sea_orm::DbErr> {
        let now = Utc::now().naive_utc();
        let id = skill.id;

        let model = ActiveModel {
            id: Set(id),
            name: Set(skill.name),
            description: Set(skill.description),
            license: Set(skill.license),
            compatibility: Set(skill.compatibility),
            metadata: Set(if skill.metadata.is_empty() {
                None
            } else {
                Some(serde_json::to_value(&skill.metadata).unwrap_or_default())
            }),
            allowed_tools: Set(skill.allowed_tools.as_ref().map(|tools| {
                serde_json::to_value(tools).unwrap_or_default()
            })),
            source_type: Set(skill.source.to_string()),
            source_url: Set(None),
            required_apis: Set(serde_json::to_value(&skill.required_apis).unwrap_or_default()),
            installed_by: Set(skill.installed_by.to_string()),
            session_id: Set(match &skill.installed_by {
                InstalledBy::Agent { session_id } => Some(session_id.clone()),
                InstalledBy::Human => None,
            }),
            created_at: Set(now),
            updated_at: Set(now),
        };

        model.insert(self.db.as_ref()).await?;
        Ok(id)
    }

    /// Look up a skill by name.
    pub async fn get(&self, name: &str) -> Result<Option<Skill>, sea_orm::DbErr> {
        let row = SkillEntity::find()
            .filter(skill::Column::Name.eq(name))
            .one(self.db.as_ref())
            .await?;
        Ok(row.map(model_to_skill))
    }

    /// List all registered skills.
    pub async fn list(&self) -> Result<Vec<Skill>, sea_orm::DbErr> {
        let rows = SkillEntity::find().all(self.db.as_ref()).await?;
        Ok(rows.into_iter().map(model_to_skill).collect())
    }

    /// Remove a skill by name. Returns `true` if it existed.
    pub async fn remove(&self, name: &str) -> Result<bool, sea_orm::DbErr> {
        let result = SkillEntity::delete_many()
            .filter(skill::Column::Name.eq(name))
            .exec(self.db.as_ref())
            .await?;
        Ok(result.rows_affected > 0)
    }
}

impl ToolRegistry for SkillRegistry {
    async fn list_tools(&self) -> Vec<ToolEntry> {
        let rows = SkillEntity::find().all(self.db.as_ref()).await;
        match rows {
            Ok(rows) => rows
                .into_iter()
                .map(|m| ToolEntry {
                    id: format!("skill/{}", m.name),
                    name: Some(m.name),
                    provider: "skill".to_string(),
                    description: Some(m.description),
                    input_schema: None,
                })
                .collect(),
            Err(_) => Vec::new(),
        }
    }
}

// ── SkillService impl ──────────────────────────────────────────────

impl SkillService for SkillRegistry {
    async fn create(
        &self,
        name: String,
        description: String,
        source: Option<String>,
        required_apis: Vec<String>,
    ) -> Result<SkillEntry, String> {
        let skill = Skill {
            id: Uuid::new_v4(),
            name,
            description,
            license: None,
            compatibility: None,
            metadata: HashMap::new(),
            allowed_tools: None,
            source: match source.as_deref() {
                Some("config") => SkillSource::Config,
                _ => SkillSource::Manual,
            },
            required_apis,
            installed_by: InstalledBy::Human,
            created_at: Utc::now().naive_utc(),
            updated_at: Utc::now().naive_utc(),
        };

        self.register(skill.clone())
            .await
            .map_err(|e| e.to_string())?;

        Ok(skill_to_entry(&skill))
    }

    async fn list(&self) -> Result<Vec<SkillEntry>, String> {
        let skills = SkillRegistry::list(self)
            .await
            .map_err(|e| e.to_string())?;
        Ok(skills.iter().map(skill_to_entry).collect())
    }

    async fn get(&self, name: &str) -> Result<Option<SkillEntry>, String> {
        let skill = SkillRegistry::get(self, name)
            .await
            .map_err(|e| e.to_string())?;
        Ok(skill.as_ref().map(skill_to_entry))
    }

    async fn delete(&self, name: &str) -> Result<bool, String> {
        self.remove(name).await.map_err(|e| e.to_string())
    }
}

fn skill_to_entry(s: &Skill) -> SkillEntry {
    SkillEntry {
        id: s.id.to_string(),
        name: s.name.clone(),
        description: s.description.clone(),
        source: s.source.to_string(),
        required_apis: s.required_apis.clone(),
        created_at: s.created_at.and_utc().to_rfc3339(),
        updated_at: s.updated_at.and_utc().to_rfc3339(),
    }
}

// ── Helpers ────────────────────────────────────────────────────────

fn model_to_skill(m: skill::Model) -> Skill {
    let metadata: HashMap<String, String> = m
        .metadata
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok())
        .unwrap_or_default();

    let allowed_tools: Option<Vec<String>> = m
        .allowed_tools
        .as_ref()
        .and_then(|v| serde_json::from_value(v.clone()).ok());

    let required_apis: Vec<String> = serde_json::from_value(m.required_apis.clone())
        .unwrap_or_default();

    let source = match m.source_type.as_str() {
        "manual" => SkillSource::Manual,
        _ => SkillSource::Config,
    };

    let installed_by = match m.installed_by.as_str() {
        "agent" => InstalledBy::Agent {
            session_id: m.session_id.unwrap_or_default(),
        },
        _ => InstalledBy::Human,
    };

    Skill {
        id: m.id,
        name: m.name,
        description: m.description,
        license: m.license,
        compatibility: m.compatibility,
        metadata,
        allowed_tools,
        source,
        required_apis,
        installed_by,
        created_at: m.created_at,
        updated_at: m.updated_at,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn test_entry(name: &str, desc: &str) -> ToolEntry {
        ToolEntry {
            id: format!("skill/{name}"),
            name: Some(name.to_string()),
            provider: "skill".to_string(),
            description: Some(desc.to_string()),
            input_schema: None,
        }
    }

    #[tokio::test]
    async fn config_skill_registry_empty() {
        let reg = ConfigSkillRegistry::new(Vec::new());
        let tools = reg.list_tools().await;
        assert!(tools.is_empty());
    }

    #[tokio::test]
    async fn config_skill_registry_returns_entries() {
        let entries = vec![
            test_entry("code-review", "Reviews code"),
            test_entry("translate", "Translates text"),
        ];

        let reg = ConfigSkillRegistry::new(entries);
        let tools = reg.list_tools().await;

        assert_eq!(tools.len(), 2);

        assert_eq!(tools[0].id, "skill/code-review");
        assert_eq!(tools[0].name.as_deref(), Some("code-review"));
        assert_eq!(tools[0].provider, "skill");
        assert_eq!(tools[0].description.as_deref(), Some("Reviews code"));
        assert!(tools[0].input_schema.is_none());

        assert_eq!(tools[1].id, "skill/translate");
        assert_eq!(tools[1].name.as_deref(), Some("translate"));
    }

    #[tokio::test]
    async fn config_skill_registry_arc_delegates() {
        let reg = Arc::new(ConfigSkillRegistry::new(vec![
            test_entry("test", "Test skill"),
        ]));

        let tools = ToolRegistry::list_tools(&*reg).await;
        assert_eq!(tools.len(), 1);
    }
}
