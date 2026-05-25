//! `/v1/budgets*` — typed CRUD wrappers over the `policies` rows whose
//! `kind = 'budget'`. Sugar over [`super::policies`] with a flat wire
//! shape and no `kind`/`spec` envelope.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::budgets`. Same
//! `policy:read` / `policy:write` scopes as the generic surface.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::types::BudgetWindow;
use super::{ManagementClient, Result};

/// One budget as it appears on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetEnvelope {
    /// Server-assigned policy id.
    pub id: String,
    /// Operator-supplied name.
    pub name: String,
    /// Rolling spend window.
    pub window: BudgetWindow,
    /// Spend cap in micro-USD.
    pub limit_micro_usd: i64,
    /// `None` for enabled rows; toggle via
    /// [`ManagementClient::disable_policy`] / `enable_policy`.
    #[serde(default)]
    pub disabled_at: Option<DateTime<Utc>>,
}

/// Wire shape returned by `GET /v1/budgets`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BudgetListResponse {
    /// One envelope per budget.
    pub data: Vec<BudgetEnvelope>,
}

/// Body for `POST /v1/budgets`.
#[derive(Debug, Clone, Serialize)]
pub struct CreateBudgetRequest {
    /// Operator-supplied display name. Must be non-empty.
    pub name: String,
    /// Rolling spend window.
    pub window: BudgetWindow,
    /// Spend cap in micro-USD. Must be strictly positive — the
    /// server refuses `<= 0` because the engine treats it as "no
    /// policy".
    pub limit_micro_usd: i64,
}

/// Body for `PUT /v1/budgets/{id}`. Each field is optional;
/// `None` ⇒ leave that column unchanged.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateBudgetRequest {
    /// New name. `None` ⇒ unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// New window. `None` ⇒ unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub window: Option<BudgetWindow>,
    /// New cap. When supplied, must be strictly positive.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub limit_micro_usd: Option<i64>,
}

/// Response from `DELETE /v1/budgets/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteBudgetResponse {
    /// Always `true` on success.
    pub deleted: bool,
}

impl ManagementClient {
    /// `GET /v1/budgets` — list every budget on the account.
    pub async fn list_budgets(&self) -> Result<BudgetListResponse> {
        self.get_json("/v1/budgets").await
    }

    /// `GET /v1/budgets/{id}` — fetch one budget. Returns
    /// [`super::Error::NotFound`] if the id belongs to a non-budget
    /// policy row.
    pub async fn get_budget(&self, id: &str) -> Result<BudgetEnvelope> {
        let path = format!("/v1/budgets/{id}");
        self.get_json(&path).await
    }

    /// `POST /v1/budgets` — create a budget.
    pub async fn create_budget(&self, body: &CreateBudgetRequest) -> Result<BudgetEnvelope> {
        self.post_json("/v1/budgets", body).await
    }

    /// `PUT /v1/budgets/{id}` — patch one or more fields of a budget.
    pub async fn update_budget(
        &self,
        id: &str,
        body: &UpdateBudgetRequest,
    ) -> Result<BudgetEnvelope> {
        let path = format!("/v1/budgets/{id}");
        self.put_json(&path, body).await
    }

    /// `DELETE /v1/budgets/{id}` — remove a budget.
    pub async fn delete_budget(&self, id: &str) -> Result<DeleteBudgetResponse> {
        let path = format!("/v1/budgets/{id}");
        self.delete_json(&path).await
    }
}
