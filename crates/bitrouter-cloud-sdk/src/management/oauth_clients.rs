//! `/v1/namespaces/{nsid}/oauth/clients*` — manage the OAuth client
//! registrations in the client's namespace.
//!
//! Mirrors `bitrouter_cloud::v1::http::management::oauth_clients`. The
//! freshly minted `client_secret` (for confidential clients) is
//! returned exactly once in the register response and never again. The
//! `{nsid}` segment is resolved from the credential's baked namespace.
//!
//! Scopes: `clients:read` / `clients:write`. `clients:write` is a
//! control-plane scope the server refuses to mint into a namespace-baked
//! CLI credential, so registering / mutating clients is console-only in
//! v1; `clients:read` is not in the default scope set
//! ([`crate::auth::settings::DEFAULT_SCOPE`]) either, so listing needs a
//! re-login with `--scope clients:read`.

use serde::{Deserialize, Serialize};

use super::types::ClientType;
use super::{ManagementClient, Result};

/// One OAuth client registration on the wire.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthClientEnvelope {
    /// Internal row id (UUID).
    pub id: String,
    /// The public `client_id` the AS advertises on flows.
    pub client_id: String,
    /// Operator-supplied display name.
    pub client_name: String,
    /// Confidential vs public — confidential clients carry a
    /// `client_secret`, public clients are PKCE-only.
    pub client_type: ClientType,
    /// Exact-match redirect URIs honoured at the authorize endpoint.
    pub redirect_uris: Vec<String>,
    /// Wire-string scopes the AS will allow this client to request.
    pub allowed_scopes: Vec<String>,
    /// Grant types this client may use (`authorization_code`,
    /// `refresh_token`, or the RFC 8628 device-code URN).
    pub allowed_grant_types: Vec<String>,
}

/// Wire shape returned by `GET /v1/oauth/clients`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct OauthClientListResponse {
    /// One envelope per client on the account.
    pub data: Vec<OauthClientEnvelope>,
}

/// Body for `POST /v1/oauth/clients`.
#[derive(Debug, Clone, Serialize)]
pub struct RegisterOauthClientRequest {
    /// Operator-supplied display name. Must be non-empty.
    pub client_name: String,
    /// Confidential vs public.
    pub client_type: ClientType,
    /// Exact-match redirect URIs. Empty for purely device-flow
    /// clients.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub redirect_uris: Vec<String>,
    /// Wire-string scopes (e.g. `"keys:read"`, `"policy:*"`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub allowed_scopes: Vec<String>,
    /// Grant types — must include at least one of
    /// `authorization_code`, `refresh_token`,
    /// `urn:ietf:params:oauth:grant-type:device_code`.
    pub allowed_grant_types: Vec<String>,
}

/// Response from `POST /v1/oauth/clients`. `client_secret` is `Some`
/// for confidential clients and is the one-and-only copy of the
/// plaintext — show once, then discard.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RegisterOauthClientResponse {
    /// Internal row id.
    pub id: String,
    /// Public `client_id`.
    pub client_id: String,
    /// One-time plaintext secret — `None` for public clients.
    pub client_secret: Option<String>,
    /// Echo of the requested display name.
    pub client_name: String,
    /// Echo of the client type.
    pub client_type: ClientType,
    /// Echo of the redirect URIs.
    pub redirect_uris: Vec<String>,
    /// Echo of the parsed scope set.
    pub allowed_scopes: Vec<String>,
    /// Echo of the parsed grant types.
    pub allowed_grant_types: Vec<String>,
}

/// Body for `PUT /v1/oauth/clients/{client_id}`. Each field is
/// independently optional — `None` ⇒ unchanged.
#[derive(Debug, Clone, Default, Serialize)]
pub struct UpdateOauthClientRequest {
    /// New display name.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_name: Option<String>,
    /// Replacement redirect-URI list (full replacement, not patch).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub redirect_uris: Option<Vec<String>>,
    /// Replacement scope set (full replacement).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_scopes: Option<Vec<String>>,
    /// Replacement grant-type list.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub allowed_grant_types: Option<Vec<String>>,
}

/// Response from `DELETE /v1/oauth/clients/{client_id}`.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeleteOauthClientResponse {
    /// Always `true` on success.
    pub deleted: bool,
}

impl ManagementClient {
    /// `GET /v1/namespaces/{nsid}/oauth/clients` — list every OAuth
    /// client in the namespace.
    pub async fn list_oauth_clients(&self) -> Result<OauthClientListResponse> {
        let path = self.namespaced("/oauth/clients")?;
        self.get_json(&path).await
    }

    /// `POST /v1/namespaces/{nsid}/oauth/clients` — register a new
    /// client. For confidential clients, the response's `client_secret`
    /// is the one-time plaintext.
    pub async fn register_oauth_client(
        &self,
        body: &RegisterOauthClientRequest,
    ) -> Result<RegisterOauthClientResponse> {
        let path = self.namespaced("/oauth/clients")?;
        self.post_json(&path, body).await
    }

    /// `PUT /v1/namespaces/{nsid}/oauth/clients/{client_id}` — patch
    /// one or more fields.
    pub async fn update_oauth_client(
        &self,
        client_id: &str,
        body: &UpdateOauthClientRequest,
    ) -> Result<OauthClientEnvelope> {
        let path = self.namespaced(&format!("/oauth/clients/{client_id}"))?;
        self.put_json(&path, body).await
    }

    /// `DELETE /v1/namespaces/{nsid}/oauth/clients/{client_id}` —
    /// remove a client.
    pub async fn delete_oauth_client(&self, client_id: &str) -> Result<DeleteOauthClientResponse> {
        let path = self.namespaced(&format!("/oauth/clients/{client_id}"))?;
        self.delete_json(&path).await
    }
}
