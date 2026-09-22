//! One content-free terminal row per started evaluation account attempt.

use sea_orm::entity::prelude::*;

#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "evaluation_attempts")]
pub struct Model {
    #[sea_orm(primary_key, auto_increment = false)]
    pub attempt_id: String,
    pub request_id: String,
    pub selector: String,
    pub provider: String,
    pub provider_model_id: String,
    pub reported_model: Option<String>,
    pub account_label: Option<String>,
    pub attempt_index: i64,
    pub format: String,
    pub duration_ms: i64,
    pub terminal: String,
    pub error_code: Option<String>,
    pub input_tokens: Option<String>,
    pub output_tokens: Option<String>,
    pub charge_status: String,
    pub charge_micro_usd: Option<i64>,
    pub charge_evidence_json: Option<String>,
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
