//! `/v1/byok/keys*` — list, upsert, and delete the caller's per-
//! provider bring-your-own-key entries.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::byok_keys`. The
//! cloud only stores already-sealed ciphertext — callers seal against
//! the cloud's current X25519 public key (fetched separately via
//! `GET /v1/byok/encryption-pubkey`; not covered by this client
//! today) and pass the base64-encoded sealed-box body in
//! [`UpsertByokKeyRequest::ciphertext_b64`].

use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};

use super::{ManagementClient, Result};

/// One BYOK key row on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByokKeyEnvelope {
    /// Upstream provider identifier (e.g. `anthropic`).
    pub provider_name: String,
    /// Id of the cloud KEK that sealed the stored ciphertext.
    pub kek_id: String,
    /// Operator-visible prefix of the underlying plaintext (e.g. for
    /// matching a leaked key to a row without leaking the secret).
    pub key_prefix: String,
    /// Override API base for the provider. `None` ⇒ provider default.
    #[serde(default)]
    pub api_base: Option<String>,
    /// When the row was created.
    pub created_at: DateTime<Utc>,
    /// When the row was last updated.
    pub updated_at: DateTime<Utc>,
    /// Last time the gateway successfully decrypted + used this key.
    #[serde(default)]
    pub last_used_at: Option<DateTime<Utc>>,
    /// Last time the gateway saw a failure attributed to this key.
    #[serde(default)]
    pub last_failure_at: Option<DateTime<Utc>>,
    /// Tag for the most recent failure (e.g. `"unauthorized"`).
    #[serde(default)]
    pub last_failure_kind: Option<String>,
}

/// Wire shape returned by `GET /v1/byok/keys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ByokKeyListResponse {
    /// One envelope per BYOK row on the account.
    pub data: Vec<ByokKeyEnvelope>,
}

/// Body for `POST /v1/byok/keys`. Idempotent — re-upserting the same
/// `provider_name` replaces the prior row.
#[derive(Debug, Clone, Serialize)]
pub struct UpsertByokKeyRequest {
    /// Upstream provider identifier.
    pub provider_name: String,
    /// Base64-encoded sealed-box ciphertext, sealed against the
    /// cloud's current X25519 public key.
    pub ciphertext_b64: String,
    /// Id of the KEK used to seal `ciphertext_b64`. Must match the
    /// cloud's current `primary_kek_id` — older KEKs are rejected at
    /// upsert time.
    pub kek_id: String,
    /// Operator-visible prefix the console renders alongside the row.
    pub key_prefix: String,
    /// Override API base for the provider. `None` ⇒ provider default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_base: Option<String>,
}

/// Response from `POST /v1/byok/keys`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct UpsertByokKeyResponse {
    /// Echo of the upserted provider id.
    pub provider_name: String,
    /// Echo of the KEK id used.
    pub kek_id: String,
    /// Echo of the key prefix.
    pub key_prefix: String,
}

/// Response from `DELETE /v1/byok/keys/{provider}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteByokKeyResponse {
    /// Always `true` on success.
    pub deleted: bool,
}

impl ManagementClient {
    /// `GET /v1/byok/keys` — list every BYOK row on the account.
    pub async fn list_byok_keys(&self) -> Result<ByokKeyListResponse> {
        self.get_json("/v1/byok/keys").await
    }

    /// `POST /v1/byok/keys` — upsert a BYOK row.
    pub async fn upsert_byok_key(
        &self,
        body: &UpsertByokKeyRequest,
    ) -> Result<UpsertByokKeyResponse> {
        self.post_json("/v1/byok/keys", body).await
    }

    /// `DELETE /v1/byok/keys/{provider}` — remove a BYOK row by
    /// provider id.
    pub async fn delete_byok_key(&self, provider: &str) -> Result<DeleteByokKeyResponse> {
        let path = format!("/v1/byok/keys/{provider}");
        self.delete_json(&path).await
    }
}
