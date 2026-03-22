//! A2A v0.3.0 security scheme types.
//!
//! Models the OpenAPI 3.2-aligned security schemes used in Agent Cards
//! for declaring authentication requirements.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

/// A security scheme declared in an Agent Card.
///
/// Mirrors the A2A v0.3.0 `SecurityScheme` oneof, serialized with a `type`
/// discriminator for JSON round-tripping.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type", rename_all = "camelCase")]
pub enum SecurityScheme {
    /// API key passed via header, query, or cookie.
    ApiKey(ApiKeySecurityScheme),
    /// HTTP authentication (Bearer, Basic, etc.).
    Http(HttpAuthSecurityScheme),
    /// OAuth 2.0 authentication.
    #[serde(rename = "oauth2")]
    OAuth2(Box<OAuth2SecurityScheme>),
    /// OpenID Connect authentication.
    OpenIdConnect(OpenIdConnectSecurityScheme),
    /// Mutual TLS authentication.
    MutualTls(MutualTlsSecurityScheme),
}

/// API key-based authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ApiKeySecurityScheme {
    /// Header, query, or cookie parameter name.
    pub name: String,
    /// Where the key is sent.
    #[serde(rename = "in")]
    pub location: ApiKeyLocation,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Location of an API key in the request.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "lowercase")]
pub enum ApiKeyLocation {
    Query,
    Header,
    Cookie,
}

/// HTTP authentication scheme (RFC 7235).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct HttpAuthSecurityScheme {
    /// HTTP authentication scheme name (e.g., `"Bearer"`, `"Basic"`).
    pub scheme: String,
    /// Bearer token format hint (e.g., `"JWT"`).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub bearer_format: Option<String>,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// OAuth 2.0 authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OAuth2SecurityScheme {
    /// Supported OAuth 2.0 flow configurations.
    pub flows: OAuthFlows,
    /// RFC 8414 authorization server metadata URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub oauth2_metadata_url: Option<String>,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// OAuth 2.0 flow configurations.
///
/// At least one flow should be specified.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct OAuthFlows {
    /// Authorization Code flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub authorization_code: Option<AuthorizationCodeOAuthFlow>,
    /// Client Credentials flow.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub client_credentials: Option<ClientCredentialsOAuthFlow>,
    /// Device Code flow (RFC 8628).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub device_code: Option<DeviceCodeOAuthFlow>,
    /// Implicit flow (deprecated in OAuth 2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub implicit: Option<ImplicitOAuthFlow>,
    /// Resource Owner Password flow (deprecated in OAuth 2.1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub password: Option<PasswordOAuthFlow>,
}

/// OAuth 2.0 Authorization Code flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct AuthorizationCodeOAuthFlow {
    /// Authorization endpoint URL.
    pub authorization_url: String,
    /// Token endpoint URL.
    pub token_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes (scope name → description).
    pub scopes: HashMap<String, String>,
    /// Whether PKCE (RFC 7636) is required.
    pub pkce_required: bool,
}

/// OAuth 2.0 Client Credentials flow.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ClientCredentialsOAuthFlow {
    /// Token endpoint URL.
    pub token_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes (scope name → description).
    pub scopes: HashMap<String, String>,
}

/// OAuth 2.0 Device Code flow (RFC 8628).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct DeviceCodeOAuthFlow {
    /// Device authorization endpoint URL.
    pub device_authorization_url: String,
    /// Token endpoint URL.
    pub token_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes (scope name → description).
    pub scopes: HashMap<String, String>,
}

/// OAuth 2.0 Implicit flow (deprecated).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct ImplicitOAuthFlow {
    /// Authorization endpoint URL.
    pub authorization_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<HashMap<String, String>>,
}

/// OAuth 2.0 Resource Owner Password flow (deprecated).
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct PasswordOAuthFlow {
    /// Token endpoint URL.
    pub token_url: String,
    /// Refresh token endpoint URL.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub refresh_url: Option<String>,
    /// Available scopes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scopes: Option<HashMap<String, String>>,
}

/// OpenID Connect authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct OpenIdConnectSecurityScheme {
    /// OpenID Connect Discovery URL.
    pub open_id_connect_url: String,
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

/// Mutual TLS authentication.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
pub struct MutualTlsSecurityScheme {
    /// Human-readable description.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn security_scheme_round_trip() {
        let scheme = SecurityScheme::Http(HttpAuthSecurityScheme {
            scheme: "Bearer".to_string(),
            bearer_format: Some("JWT".to_string()),
            description: None,
        });

        let json = serde_json::to_string(&scheme).expect("serialize");
        let parsed: SecurityScheme = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(scheme, parsed);
    }

    #[test]
    fn api_key_scheme_round_trip() {
        let scheme = SecurityScheme::ApiKey(ApiKeySecurityScheme {
            name: "X-API-Key".to_string(),
            location: ApiKeyLocation::Header,
            description: Some("API key header".to_string()),
        });

        let json = serde_json::to_string(&scheme).expect("serialize");
        let parsed: SecurityScheme = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(scheme, parsed);
    }

    #[test]
    fn oauth2_flows_round_trip() {
        let flows = OAuthFlows {
            authorization_code: Some(AuthorizationCodeOAuthFlow {
                authorization_url: "https://auth.example.com/authorize".to_string(),
                token_url: "https://auth.example.com/token".to_string(),
                refresh_url: None,
                scopes: HashMap::from([("read".to_string(), "Read access".to_string())]),
                pkce_required: true,
            }),
            client_credentials: None,
            device_code: None,
            implicit: None,
            password: None,
        };

        let json = serde_json::to_string(&flows).expect("serialize");
        let parsed: OAuthFlows = serde_json::from_str(&json).expect("deserialize");
        assert_eq!(flows, parsed);
    }
}
