//! `/v1/namespaces/{nsid}/keys` — list, mint, and revoke `brk_` API keys
//! in the client's namespace.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::keys`. Scopes:
//! `keys:read` for list, `keys:write` for mint and revoke. The `{nsid}`
//! segment is resolved from the credential's baked namespace.

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ManagementClient, Result};

/// Wire shape returned by `GET /v1/keys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyListResponse {
    /// One envelope per key on the account.
    pub data: Vec<ApiKeyEnvelope>,
}

/// One row of the `api_keys` table, read-side projection.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiKeyEnvelope {
    /// Server-assigned identifier (the `id` column).
    pub id: String,
    /// Operator-supplied friendly name.
    pub display_name: String,
    /// First few characters of the plaintext token, for matching a
    /// leaked secret to its row without storing the secret.
    pub key_prefix: String,
    /// Scopes granted to this key, in wire-string form
    /// (e.g. `"keys:read"`, `"policy:*"`).
    pub scopes: Vec<String>,
    /// Optional expiry. Past expiry the key is refused at the
    /// gateway.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
    /// Last time the gateway saw this key.
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    /// When the key was revoked, if ever.
    #[serde(default)]
    pub revoked_at: Option<DateTime<Utc>>,
    /// When the row was created.
    pub created_at: DateTime<Utc>,
}

/// Body for `POST /v1/keys`.
#[derive(Debug, Clone, Serialize)]
pub struct MintApiKeyRequest {
    /// Operator-supplied friendly name. Must be non-empty.
    pub display_name: String,
    /// Requested scopes (wire-string form). Each must be a subset of
    /// the caller's effective scopes — RFC 6749 §3.3 forbids
    /// upscaling.
    pub scopes: Vec<String>,
    /// Optional expiry. Omit for a non-expiring key.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Response from `POST /v1/keys`. `token` is the **one-time** plaintext
/// — the server has no copy after this response.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MintApiKeyResponse {
    /// Plaintext `brk_…` token. Show once, then discard.
    pub token: String,
    /// Server-assigned id for the persisted row.
    pub id: String,
    /// Visible prefix of the token, persisted for the row.
    pub key_prefix: String,
    /// Echo of the requested display name.
    pub display_name: String,
    /// Echo of the requested scopes (after parsing).
    pub scopes: Vec<String>,
    /// Echo of the requested expiry.
    #[serde(default)]
    pub expires_at: Option<DateTime<Utc>>,
}

/// Response from `DELETE /v1/keys/{id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RevokeApiKeyResponse {
    /// Always `true` on success — the server returns 404 (mapped to
    /// [`super::Error::NotFound`]) when the row doesn't exist or
    /// belongs to a different namespace.
    pub revoked: bool,
}

impl ManagementClient {
    /// `GET /v1/namespaces/{nsid}/keys` — list API keys in the client's
    /// namespace.
    pub async fn list_keys(&self) -> Result<ApiKeyListResponse> {
        let path = self.namespaced("/keys")?;
        self.get_json(&path).await
    }

    /// `POST /v1/namespaces/{nsid}/keys` — mint a new API key. The
    /// returned `token` is the only copy of the plaintext.
    pub async fn mint_key(&self, body: &MintApiKeyRequest) -> Result<MintApiKeyResponse> {
        let path = self.namespaced("/keys")?;
        self.post_json(&path, body).await
    }

    /// `DELETE /v1/namespaces/{nsid}/keys/{id}` — revoke a key by id.
    pub async fn revoke_key(&self, id: &str) -> Result<RevokeApiKeyResponse> {
        let path = self.namespaced(&format!("/keys/{id}"))?;
        self.delete_json(&path).await
    }
}
