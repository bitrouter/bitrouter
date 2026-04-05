//! Spend log entity for persisting per-request/tool/agent cost data.

use chrono::NaiveDateTime;
use sea_orm::entity::prelude::*;
use uuid::Uuid;

#[derive(Clone, Debug, PartialEq, DeriveEntityModel)]
#[sea_orm(table_name = "spend_logs")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub id: Uuid,
    pub service_type: String,
    pub account_id: Option<String>,
    pub key_id: Option<String>,
    pub session_id: Option<Uuid>,
    pub service_name: String,
    pub operation: String,
    pub input_tokens: i32,
    pub output_tokens: i32,
    pub cost: f64,
    pub latency_ms: i64,
    pub success: bool,
    pub error_info: Option<String>,
    pub created_at: NaiveDateTime,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
