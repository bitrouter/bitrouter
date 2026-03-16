//! Spend log entity for persisting per-request cost data.

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "spend_logs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub account_id: Option<String>,
    pub model: String,
    pub provider: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cost: f64,
    pub latency_ms: i64,
    pub success: bool,
    pub error_type: Option<String>,
    pub created_at: NaiveDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
