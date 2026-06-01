//! `/v1/namespaces/{nsid}/policies*` — generic CRUD over the typed
//! policy registry plus binding, enable/disable, per-principal listing,
//! and the effective-policy preview.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::policies`. Scopes:
//! `policy:read` for reads, `policy:write` for everything else. The
//! `{nsid}` segment is resolved from the credential's baked namespace.
//!
//! The shape used for `spec` is the **flat inner body** (e.g. `{
//! "window": "day", "limit_micro_usd": 1000 }` for a budget), not the
//! tagged outer form — see the server doc-comment on
//! `bitrouter_cloud::policy::spec::PolicySpec::from_row`. Callers
//! either pass typed sub-structs from [`super::types`] or a raw
//! [`serde_json::Value`] (e.g. for guardrails / rate-limits which we
//! don't yet model as Rust structs in this crate).

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::types::PolicyKind;
use super::{ManagementClient, Result};

/// One row from `GET /v1/policies` or its single-row variants.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyEnvelope {
    /// Server-assigned identifier.
    pub id: String,
    /// Operator-supplied name.
    pub name: String,
    /// Discriminator for the row's spec body.
    pub kind: PolicyKind,
    /// Flat inner spec body — caller-typed downstream.
    pub spec: serde_json::Value,
    /// `None` for enabled rows; `Some` once an operator has parked
    /// the policy via `disable_policy`. Disabled rows are still
    /// returned by reads; the engine skips them at request time.
    #[serde(default)]
    pub disabled_at: Option<DateTime<Utc>>,
}

/// Wire shape returned by every list-style policy endpoint.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PolicyListResponse {
    /// One envelope per policy.
    pub data: Vec<PolicyEnvelope>,
}

/// Query string for `GET /v1/policies`.
#[derive(Debug, Clone, Default, Serialize)]
pub struct ListPoliciesQuery {
    /// Narrow the result to a single kind. `None` returns every kind.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub kind: Option<PolicyKind>,
}

/// Body for `POST /v1/policies`.
#[derive(Debug, Clone, Serialize)]
pub struct CreatePolicyRequest {
    /// Operator-supplied display name. Must be non-empty.
    pub name: String,
    /// Discriminator — selects which shape `spec` must take.
    pub kind: PolicyKind,
    /// Flat inner spec body. Server validates against `kind`.
    pub spec: serde_json::Value,
}

/// Body for `PUT /v1/policies/{id}`. Both fields are optional —
/// `None` leaves the column untouched. Kind changes aren't supported;
/// when supplied, the spec must serialise against the row's existing
/// kind.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdatePolicyRequest {
    /// New name. `None` ⇒ unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// New spec. `None` ⇒ unchanged.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<serde_json::Value>,
}

/// Response for `DELETE /v1/policies/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeletePolicyResponse {
    /// Always `true` on success (404 maps to
    /// [`super::Error::NotFound`]).
    pub deleted: bool,
}

/// Body for `POST /v1/policies/{id}/bind`.
#[derive(Debug, Clone, Serialize)]
pub struct BindPolicyRequest {
    /// One of `namespace`, `api_key`, `oauth_token`, `oauth_client`.
    pub principal_type: String,
    /// Id of the principal — interpretation depends on
    /// `principal_type`.
    pub principal_id: String,
}

/// Response for `POST /v1/policies/{id}/bind`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindPolicyResponse {
    /// Id of the new binding row, addressable by `DELETE
    /// /v1/policies/{id}/bind/{binding_id}`.
    pub binding_id: String,
}

/// Response for `DELETE /v1/policies/{id}/bind/{binding_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UnbindPolicyResponse {
    /// Always `true` on success.
    pub unbound: bool,
}

/// Response for `POST /v1/policies/{id}/disable` and
/// `/v1/policies/{id}/enable`. `disabled` reflects the **post-call**
/// state (idempotent — re-disabling returns the same body).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToggleResponse {
    /// `true` when the policy is now disabled.
    pub disabled: bool,
}

/// One binding row, as returned by
/// `GET /v1/policies/{id}/bindings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingEnvelope {
    /// Binding row id.
    pub id: String,
    /// Owning policy id.
    pub policy_id: String,
    /// One of `namespace`, `api_key`, `oauth_token`, `oauth_client`.
    pub principal_type: String,
    /// Id of the principal this binding targets.
    pub principal_id: String,
}

/// Wire shape returned by `GET /v1/policies/{id}/bindings`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct BindingListResponse {
    /// One envelope per binding on the policy.
    pub data: Vec<BindingEnvelope>,
}

/// Query string for `GET /v1/policies/effective`. Both fields are
/// required: there's no "for me" shortcut because the engine
/// computes namespace-vs-credential composition differently.
#[derive(Debug, Clone, Serialize)]
pub struct EffectivePolicyQuery {
    /// One of `namespace`, `api_key`, `oauth_token`, `oauth_client`.
    pub principal_type: String,
    /// Id of the principal the preview is computed for.
    pub principal_id: String,
}

/// Wire shape returned by `GET /v1/policies/effective`. Mirrors
/// `bitrouter_cloud::policy::engine::EffectivePolicy` — a flat list
/// per kind, plus a single guardrail composed under most-restrictive-
/// wins.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct EffectivePolicy {
    /// All budget clauses that would apply to a request.
    #[serde(default)]
    pub budgets: Vec<serde_json::Value>,
    /// All rate-limit clauses that would apply.
    #[serde(default)]
    pub rate_limits: Vec<serde_json::Value>,
    /// Combined guardrail (most-restrictive-wins). `None` means no
    /// guardrail policy applied.
    #[serde(default)]
    pub guardrail: Option<serde_json::Value>,
}

impl ManagementClient {
    /// `GET /v1/namespaces/{nsid}/policies` — list policies in the
    /// client's namespace, optionally narrowed by kind.
    pub async fn list_policies(&self, query: &ListPoliciesQuery) -> Result<PolicyListResponse> {
        let path = self.namespaced("/policies")?;
        self.get_with_query(&path, query).await
    }

    /// `GET /v1/namespaces/{nsid}/policies/{id}` — fetch a single
    /// policy.
    pub async fn get_policy(&self, id: &str) -> Result<PolicyEnvelope> {
        let path = self.namespaced(&format!("/policies/{id}"))?;
        self.get_json(&path).await
    }

    /// `POST /v1/namespaces/{nsid}/policies` — create a new policy.
    pub async fn create_policy(&self, body: &CreatePolicyRequest) -> Result<PolicyEnvelope> {
        let path = self.namespaced("/policies")?;
        self.post_json(&path, body).await
    }

    /// `PUT /v1/namespaces/{nsid}/policies/{id}` — patch name and/or
    /// spec.
    pub async fn update_policy(
        &self,
        id: &str,
        body: &UpdatePolicyRequest,
    ) -> Result<PolicyEnvelope> {
        let path = self.namespaced(&format!("/policies/{id}"))?;
        self.put_json(&path, body).await
    }

    /// `DELETE /v1/namespaces/{nsid}/policies/{id}` — remove a policy.
    /// Cascades to its bindings server-side.
    pub async fn delete_policy(&self, id: &str) -> Result<DeletePolicyResponse> {
        let path = self.namespaced(&format!("/policies/{id}"))?;
        self.delete_json(&path).await
    }

    /// `POST /v1/namespaces/{nsid}/policies/{id}/bind` — attach a
    /// policy to a principal.
    pub async fn bind_policy(
        &self,
        id: &str,
        body: &BindPolicyRequest,
    ) -> Result<BindPolicyResponse> {
        let path = self.namespaced(&format!("/policies/{id}/bind"))?;
        self.post_json(&path, body).await
    }

    /// `DELETE /v1/namespaces/{nsid}/policies/{id}/bind/{binding_id}` —
    /// detach one binding.
    pub async fn unbind_policy(&self, id: &str, binding_id: &str) -> Result<UnbindPolicyResponse> {
        let path = self.namespaced(&format!("/policies/{id}/bind/{binding_id}"))?;
        self.delete_json(&path).await
    }

    /// `GET /v1/namespaces/{nsid}/policies/{id}/bindings` — list
    /// bindings for one policy.
    pub async fn list_policy_bindings(&self, id: &str) -> Result<BindingListResponse> {
        let path = self.namespaced(&format!("/policies/{id}/bindings"))?;
        self.get_json(&path).await
    }

    /// `POST /v1/namespaces/{nsid}/policies/{id}/disable` — park a
    /// policy. The row stays in the table; the engine skips it at
    /// request time.
    pub async fn disable_policy(&self, id: &str) -> Result<ToggleResponse> {
        let path = self.namespaced(&format!("/policies/{id}/disable"))?;
        self.post_empty(&path).await
    }

    /// `POST /v1/namespaces/{nsid}/policies/{id}/enable` — un-park a
    /// previously disabled policy.
    pub async fn enable_policy(&self, id: &str) -> Result<ToggleResponse> {
        let path = self.namespaced(&format!("/policies/{id}/enable"))?;
        self.post_empty(&path).await
    }

    /// `GET /v1/namespaces/{nsid}/principals/{type}/{id}/policies` —
    /// every policy bound to a given principal.
    pub async fn list_principal_policies(
        &self,
        principal_type: &str,
        principal_id: &str,
    ) -> Result<PolicyListResponse> {
        let path = self.namespaced(&format!(
            "/principals/{principal_type}/{principal_id}/policies"
        ))?;
        self.get_json(&path).await
    }

    /// `GET /v1/namespaces/{nsid}/policies/effective` — preview the
    /// effective policy for a principal without making a real inference
    /// call.
    pub async fn effective_policy(&self, query: &EffectivePolicyQuery) -> Result<EffectivePolicy> {
        let path = self.namespaced("/policies/effective")?;
        self.get_with_query(&path, query).await
    }
}
