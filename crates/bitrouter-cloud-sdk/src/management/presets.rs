//! `/v1/namespaces/{nsid}/presets*` — typed CRUD wrappers over the
//! `policies` rows whose `kind = 'preset'`. A preset is a named bundle
//! of an optional `guardrail`, `budget`, and / or `rate_limit` clause.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::presets`. The
//! `{nsid}` segment is resolved from the credential's baked namespace.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ManagementClient, Result};

/// One preset on the wire. The sub-policy fields are optional so an
/// empty preset (no clauses) is representable; the engine ignores
/// empty presets at request time.
///
/// `guardrail` / `budget` / `rate_limit` are deserialised as raw
/// [`serde_json::Value`] for simplicity — callers that want strong
/// typing for the inner clauses can do `serde_json::from_value`
/// against e.g. `crate::management::types::BudgetWindow`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetEnvelope {
    /// Server-assigned policy id.
    pub id: String,
    /// Operator-supplied name.
    pub name: String,
    /// Guardrail clause, if set.
    #[serde(default)]
    pub guardrail: Option<serde_json::Value>,
    /// Budget clause, if set.
    #[serde(default)]
    pub budget: Option<serde_json::Value>,
    /// Rate-limit clause, if set.
    #[serde(default)]
    pub rate_limit: Option<serde_json::Value>,
    /// `None` for enabled rows.
    #[serde(default)]
    pub disabled_at: Option<DateTime<Utc>>,
}

/// Wire shape returned by `GET /v1/presets`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PresetListResponse {
    /// One envelope per preset.
    pub data: Vec<PresetEnvelope>,
}

/// Body for `POST /v1/presets`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct CreatePresetRequest {
    /// Operator-supplied display name. Must be non-empty.
    pub name: String,
    /// Optional guardrail clause (server-validated against
    /// `GuardrailSpec`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guardrail: Option<serde_json::Value>,
    /// Optional budget clause.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<serde_json::Value>,
    /// Optional rate-limit clause.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<serde_json::Value>,
}

/// Body for `PUT /v1/presets/{id}`. Each clause field is independently
/// optional. To **drop** a clause from a preset, set the matching
/// `clear_*` flag — the server does not honour JSON `null` for this.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdatePresetRequest {
    /// New name. `None` ⇒ unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Replace the guardrail clause. `None` ⇒ unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub guardrail: Option<serde_json::Value>,
    /// Replace the budget clause. `None` ⇒ unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub budget: Option<serde_json::Value>,
    /// Replace the rate-limit clause. `None` ⇒ unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub rate_limit: Option<serde_json::Value>,
    /// When `true`, drop the guardrail clause from the stored preset.
    /// Overrides `guardrail` if both are present.
    #[serde(default, skip_serializing_if = "is_false")]
    pub clear_guardrail: bool,
    /// When `true`, drop the budget clause.
    #[serde(default, skip_serializing_if = "is_false")]
    pub clear_budget: bool,
    /// When `true`, drop the rate-limit clause.
    #[serde(default, skip_serializing_if = "is_false")]
    pub clear_rate_limit: bool,
}

fn is_false(b: &bool) -> bool {
    !*b
}

/// Response from `DELETE /v1/presets/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletePresetResponse {
    /// Always `true` on success.
    pub deleted: bool,
}

impl ManagementClient {
    /// `GET /v1/namespaces/{nsid}/presets` — list every preset in the
    /// namespace.
    pub async fn list_presets(&self) -> Result<PresetListResponse> {
        let path = self.namespaced("/presets")?;
        self.get_json(&path).await
    }

    /// `GET /v1/namespaces/{nsid}/presets/{id}` — fetch one preset.
    pub async fn get_preset(&self, id: &str) -> Result<PresetEnvelope> {
        let path = self.namespaced(&format!("/presets/{id}"))?;
        self.get_json(&path).await
    }

    /// `POST /v1/namespaces/{nsid}/presets` — create a preset.
    pub async fn create_preset(&self, body: &CreatePresetRequest) -> Result<PresetEnvelope> {
        let path = self.namespaced("/presets")?;
        self.post_json(&path, body).await
    }

    /// `PUT /v1/namespaces/{nsid}/presets/{id}` — patch a preset's name
    /// and/or clauses.
    pub async fn update_preset(
        &self,
        id: &str,
        body: &UpdatePresetRequest,
    ) -> Result<PresetEnvelope> {
        let path = self.namespaced(&format!("/presets/{id}"))?;
        self.put_json(&path, body).await
    }

    /// `DELETE /v1/namespaces/{nsid}/presets/{id}` — remove a preset.
    pub async fn delete_preset(&self, id: &str) -> Result<DeletePresetResponse> {
        let path = self.namespaced(&format!("/presets/{id}"))?;
        self.delete_json(&path).await
    }
}
