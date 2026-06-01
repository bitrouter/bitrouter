//! `/v1/namespaces` — list the namespaces the signed-in user owns.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::namespaces`. This is a
//! user-level endpoint (keyed on the subject user server-side, not the
//! credential's baked namespace), so it works for any signed-in
//! credential and needs no `{nsid}` segment.
//!
//! Only the read path is exposed. Creating and deleting namespaces
//! requires the control-plane `namespace:write` scope, which the server
//! refuses to mint into a namespace-baked CLI credential — that lifecycle
//! is console-only in v1. The CLI uses this list to show which namespaces
//! exist (and which one the current credential is bound to) so a user can
//! decide whether to re-login against a different namespace.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ManagementClient, Result};

/// One namespace the caller owns, as returned by `GET /v1/namespaces`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceEnvelope {
    /// Server-assigned namespace id (the `{nsid}` used in every
    /// namespace-scoped path).
    pub id: String,
    /// Operator-supplied name, unique per owner.
    pub name: String,
    /// When the namespace was created.
    pub created_at: DateTime<Utc>,
}

/// Wire shape returned by `GET /v1/namespaces`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct NamespaceListResponse {
    /// One envelope per namespace the caller owns.
    pub data: Vec<NamespaceEnvelope>,
}

impl ManagementClient {
    /// `GET /v1/namespaces` — list every namespace the signed-in user
    /// owns. Requires `namespace:read` (in the default scope set).
    pub async fn list_namespaces(&self) -> Result<NamespaceListResponse> {
        self.get_json("/v1/namespaces").await
    }
}
