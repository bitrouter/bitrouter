//! Skill entity.

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "skills")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    #[sea_orm(unique)]
    pub name: String,
    #[sea_orm(column_type = "Text")]
    pub description: String,
    pub license: Option<String>,
    pub compatibility: Option<String>,
    /// Arbitrary key-value metadata (JSON).
    #[sea_orm(column_type = "Json", nullable)]
    pub metadata: Option<serde_json::Value>,
    /// Pre-approved tool names (JSON array).
    #[sea_orm(column_type = "Json", nullable)]
    pub allowed_tools: Option<serde_json::Value>,
    /// "config" or "manual".
    pub source_type: String,
    pub source_url: Option<String>,
    /// Provider names this skill depends on (JSON array of strings).
    #[sea_orm(column_type = "Json")]
    pub required_apis: serde_json::Value,
    /// "human" or "agent".
    pub installed_by: String,
    /// Session ID when installed by an agent.
    pub session_id: Option<String>,
    pub created_at: NaiveDateTime,
    pub updated_at: NaiveDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
