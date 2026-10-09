//! Origin-bound hosted credentials independent of file paths and account selection.

use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};

/// One stored OAuth credential set.
///
/// `Debug` redacts `access_token` and `refresh_token` so a stray
/// `tracing::error!(?credentials, …)` can never dump the bearer to the
/// log stream.
#[derive(Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Credentials {
    /// RFC 6749 §1.4 bearer token. Sent on outgoing requests as
    /// `Authorization: Bearer <access_token>` (RFC 6750 §2.1).
    pub access_token: String,
    /// RFC 6749 §1.5 refresh token. Optional — the AS may decline to
    /// issue one, or `bro cloud login` may have been run with a
    /// scope the AS refuses to refresh.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token: Option<String>,
    /// Wall-clock UTC at which `access_token` becomes invalid.
    pub expires_at: DateTime<Utc>,
    /// Wall-clock UTC at which `refresh_token` itself becomes invalid.
    /// Optional — many AS deployments issue refresh tokens with no
    /// declared expiry; absent here means "treat the refresh token as
    /// valid until the AS itself rejects it".
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub refresh_token_expires_at: Option<DateTime<Utc>>,
    /// RFC 6749 §7.1 token type. Bitrouter only supports `Bearer` today.
    pub token_type: String,
    /// RFC 6749 §3.3 scope — the space-delimited list of scopes the AS
    /// granted (which may be narrower than what was requested).
    pub scope: String,
    /// The client id the device-flow was run as. Captured so a later
    /// request-time refresh uses the same client id even if the
    /// env var has changed since.
    pub client_id: String,
    /// The AS base URL the device-flow was run against. Captured for the
    /// same reason as `client_id`.
    pub authorization_server: String,
    /// Namespace the credential is baked into. Every device-flow token
    /// the CLI obtains is namespace-baked, so this is normally `Some`.
    /// Absent for a namespace-null credential (the console web session,
    /// which the CLI never holds) — and for a credential file written
    /// before namespace-scoping shipped, where it signals "re-login to
    /// get a namespace-scoped token". Read once at client construction
    /// to resolve the implicit `{nsid}` in management calls; preserved
    /// verbatim across refreshes since rotation never rebinds the
    /// namespace.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub namespace_id: Option<String>,
    /// Subject identifier returned by the AS (typically `sub` from an
    /// OpenID Connect ID token). Optional — populated when the AS
    /// returned an `id_token` claim the flow could decode. Used by
    /// `whoami`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub subject: Option<String>,
}

impl std::fmt::Debug for Credentials {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Credentials")
            .field("access_token", &"<redacted>")
            .field(
                "refresh_token",
                &self.refresh_token.as_ref().map(|_| "<redacted>"),
            )
            .field("expires_at", &self.expires_at)
            .field("refresh_token_expires_at", &self.refresh_token_expires_at)
            .field("token_type", &self.token_type)
            .field("scope", &self.scope)
            .field("client_id", &self.client_id)
            .field("authorization_server", &self.authorization_server)
            .field("namespace_id", &self.namespace_id)
            .field("subject", &self.subject)
            .finish()
    }
}

impl Credentials {
    /// Is `access_token` still within its TTL at the current wall clock?
    pub fn access_token_valid(&self) -> bool {
        Utc::now() < self.expires_at
    }

    /// Is the access token within `window` of expiring (or already
    /// expired)? Used by [`super::session::HostedSession`] to trigger
    /// a refresh slightly before the token actually becomes invalid.
    pub fn access_token_near_expiry(&self, window: Duration) -> bool {
        Utc::now() + window >= self.expires_at
    }

    /// Has the refresh token itself expired? `None` is treated as
    /// "valid forever" — many AS deployments omit `refresh_token_expires_in`.
    pub fn refresh_token_usable(&self) -> bool {
        match self.refresh_token_expires_at {
            Some(t) => Utc::now() < t,
            None => self.refresh_token.is_some(),
        }
    }
}

/// Authentication mechanism represented by a stored credential.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CredentialKind {
    /// OAuth 2.0 access and refresh tokens.
    Oauth,
    /// A static BitRouter Cloud API key.
    ApiKey,
}

/// A credential persisted by `bro cloud login`.
///
/// New files use a tagged representation. Deserialization also accepts the
/// original untagged OAuth object so existing logins continue to work.
#[derive(Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum StoredCredential {
    /// OAuth 2.0 device-flow credentials.
    Oauth {
        /// The persisted OAuth token set and its refresh metadata.
        #[serde(flatten)]
        credential: Credentials,
    },
    /// A static API key bound to the login origin.
    ApiKey {
        /// The bearer value. Debug output always redacts this field.
        api_key: String,
        /// The BitRouter Cloud origin selected at login time.
        base_url: String,
    },
}

#[derive(Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
enum TaggedCredential {
    Oauth {
        #[serde(flatten)]
        credential: Credentials,
    },
    ApiKey {
        api_key: String,
        base_url: String,
    },
}

#[derive(Deserialize)]
#[serde(untagged)]
enum CompatibleCredential {
    Tagged(TaggedCredential),
    LegacyOauth(Credentials),
}

impl<'de> Deserialize<'de> for StoredCredential {
    fn deserialize<D>(deserializer: D) -> std::result::Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        match CompatibleCredential::deserialize(deserializer)? {
            CompatibleCredential::Tagged(TaggedCredential::Oauth { credential })
            | CompatibleCredential::LegacyOauth(credential) => Ok(Self::Oauth { credential }),
            CompatibleCredential::Tagged(TaggedCredential::ApiKey { api_key, base_url }) => {
                Ok(Self::ApiKey { api_key, base_url })
            }
        }
    }
}

impl std::fmt::Debug for StoredCredential {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Oauth { credential } => f
                .debug_struct("StoredCredential::Oauth")
                .field("credential", credential)
                .finish(),
            Self::ApiKey { base_url, .. } => f
                .debug_struct("StoredCredential::ApiKey")
                .field("api_key", &"<redacted>")
                .field("base_url", base_url)
                .finish(),
        }
    }
}

impl From<Credentials> for StoredCredential {
    fn from(credential: Credentials) -> Self {
        Self::Oauth { credential }
    }
}

impl StoredCredential {
    /// Construct a static API-key credential for `base_url`.
    pub fn api_key(api_key: String, base_url: String) -> Self {
        Self::ApiKey { api_key, base_url }
    }

    /// Return the authentication mechanism used by this credential.
    pub fn kind(&self) -> CredentialKind {
        match self {
            Self::Oauth { .. } => CredentialKind::Oauth,
            Self::ApiKey { .. } => CredentialKind::ApiKey,
        }
    }

    /// Return the BitRouter Cloud base URL recorded at login time.
    pub fn base_url(&self) -> &str {
        match self {
            Self::Oauth { credential } => &credential.authorization_server,
            Self::ApiKey { base_url, .. } => base_url,
        }
    }

    /// Return the OAuth payload, or `None` for a static API key.
    pub fn oauth(&self) -> Option<&Credentials> {
        match self {
            Self::Oauth { credential } => Some(credential),
            Self::ApiKey { .. } => None,
        }
    }

    /// Return the namespace bound to an OAuth credential.
    pub fn namespace_id(&self) -> Option<&str> {
        self.oauth()
            .and_then(|credential| credential.namespace_id.as_deref())
    }

    /// Return the OAuth scope, or `None` for a static API key.
    pub fn scope(&self) -> Option<&str> {
        self.oauth().map(|credential| credential.scope.as_str())
    }

    /// Return the OAuth subject, or `None` for a static API key.
    pub fn subject(&self) -> Option<&str> {
        self.oauth()
            .and_then(|credential| credential.subject.as_deref())
    }
}

/// Refresh shortly before access-token expiry.
pub const REFRESH_WINDOW: Duration = Duration::seconds(60);
