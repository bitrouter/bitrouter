//! Typed HTTP client for the BitRouter Cloud `/v1/*` management surface.
//!
//! The surface — `keys`, `usage`, `billing`, `policies`, `budgets`,
//! `presets`, `byok`, `oauth_clients` — is the same one the web console
//! consumes. It accepts either a `brk_` API key or a `bra_` OAuth
//! access token. This client always presents the latter: it reads the
//! credential persisted by `bitrouter auth login`
//! ([`crate::auth::credentials::CredentialsStore`]) and refreshes it
//! transparently within
//! [`crate::auth::credentials::REFRESH_WINDOW`] of expiry.
//!
//! ## Namespace scoping
//!
//! The server bifurcates the management surface (see
//! `bitrouter_cloud::v1::http::management`): namespace-scoped endpoints
//! live under `/v1/namespaces/{nsid}/…`, user-level endpoints stay flat.
//! The CLI's credential is namespace-baked, so the client captures its
//! `namespace_id` at construction and resolves the `{nsid}` segment
//! implicitly — callers never pass a namespace argument. User-level
//! endpoints (`namespaces`, `billing`, `byok`) ignore the namespace and
//! key on the subject user server-side.
//!
//! ## Endpoint coverage
//!
//! Methods are split across per-resource modules and exposed as
//! additional `impl ManagementClient` blocks. Namespace-scoped (✱) vs
//! user-level:
//!
//! - [`namespaces`] — `/v1/namespaces` (list, read-only)
//! - [`keys`] ✱ — `/v1/namespaces/{nsid}/keys`
//! - [`usage`] ✱ — `…/usage`, `…/requests`
//! - [`billing`] — `/v1/billing/*`
//! - [`policies`] ✱ — `…/policies*`, including `…/policies/effective`
//!   and per-principal listing
//! - [`budgets`] ✱ — `…/budgets*` (typed sugar)
//! - [`presets`] ✱ — `…/presets*` (typed sugar)
//! - [`byok`] — `/v1/byok/keys*`
//! - [`oauth_clients`] ✱ — `…/oauth/clients*`
//!
//! ## Errors
//!
//! Every method returns [`Result<T>`] where the error is the
//! single-typed [`enum@Error`] mirroring the server's wire-error taxonomy
//! plus the local failure modes (no credentials, transport, decode). A
//! 403 with the server's `missing required scope: <s>` body shape is
//! parsed into [`Error::Forbidden`] with `missing_scope = Some(s)` so
//! the CLI can suggest a re-login with the missing scope appended.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;
use tokio::sync::Mutex;

use crate::auth::credentials::{CredentialsStore, default_credentials_path};
use crate::auth::metadata::{self, AsMetadata};

pub mod billing;
pub mod budgets;
pub mod byok;
pub mod error;
pub mod keys;
pub mod namespaces;
pub mod oauth_clients;
pub mod policies;
pub mod presets;
pub mod types;
pub mod usage;

#[cfg(test)]
mod tests;

pub use error::Error;

/// Convenience `Result` alias used by every method on
/// [`ManagementClient`].
pub type Result<T> = std::result::Result<T, Error>;

/// Typed client for the BitRouter Cloud `/v1/*` management surface.
///
/// Construct via [`ManagementClient::from_default_credentials`] (which
/// reads `<data-dir>/account-credentials.json`) and call the per-method
/// helpers defined in this module's sub-modules.
#[derive(Debug)]
pub struct ManagementClient {
    /// Cloud base URL (no trailing slash). The credentials file's
    /// `authorization_server` field is the source of truth — `bitrouter
    /// auth login` against `https://my-self-hosted.example.com` makes
    /// this client target that same host.
    base_url: String,
    http: reqwest::Client,
    /// Locked across the disk-read → refresh → persist sequence per RFC
    /// 6749 §6 rotation safety. A concurrent refresh from another task
    /// would race the AS into invalidating the older refresh token —
    /// the mutex serialises that path. Matches the pattern used by
    /// [`crate::provider::BitrouterCloudAuthApplier`].
    store: Arc<Mutex<CredentialsStore>>,
    /// AS metadata cached for the process lifetime. The AS URL is
    /// captured at construction so a re-login against a different AS
    /// implicitly invalidates the cache (the next `ManagementClient`
    /// instance reads the new URL from disk).
    metadata: Arc<Mutex<Option<AsMetadata>>>,
    /// The namespace the stored credential is baked into, captured at
    /// construction. `Some` for every device-flow token; `None` only
    /// for a namespace-null credential or a pre-namespace credential
    /// file. Namespace-scoped methods resolve the `{nsid}` path segment
    /// from this via [`ManagementClient::namespaced`]. Immutable for the
    /// client's lifetime — refresh never rebinds the namespace.
    namespace_id: Option<String>,
}

impl ManagementClient {
    /// Build a client from the default credentials path
    /// (`<data-dir>/account-credentials.json`). Errors with
    /// [`Error::NotSignedIn`] when the file is absent so callers can
    /// print the onboarding hint without a stack trace.
    pub fn from_default_credentials() -> Result<Self> {
        let path = default_credentials_path()
            .context("resolving BitRouter Cloud credentials path")
            .map_err(Error::Auth)?;
        Self::from_credentials_path(path)
    }

    /// Build a client reading the credentials file at `path`. Used by
    /// tests to point at a temporary directory and by callers that
    /// override the default location.
    pub fn from_credentials_path(path: PathBuf) -> Result<Self> {
        let store = CredentialsStore::load(&path)
            .with_context(|| format!("reading credentials at {}", path.display()))
            .map_err(Error::Auth)?;
        let creds = store.current().ok_or(Error::NotSignedIn)?;
        let base_url = creds.authorization_server.trim_end_matches('/').to_owned();
        let namespace_id = creds.namespace_id.clone();
        let http = build_http_client()?;
        Ok(Self {
            base_url,
            http,
            store: Arc::new(Mutex::new(store)),
            metadata: Arc::new(Mutex::new(None)),
            namespace_id,
        })
    }

    /// Construct with an explicit base URL and HTTP client. Used by
    /// the wiremock test harness so a single mock server stands in
    /// for both the AS metadata + token endpoints and the `/v1/*`
    /// management endpoints.
    #[cfg(test)]
    pub(crate) fn with_parts(
        base_url: String,
        http: reqwest::Client,
        store: CredentialsStore,
    ) -> Self {
        let namespace_id = store.current().and_then(|c| c.namespace_id.clone());
        Self {
            base_url: base_url.trim_end_matches('/').to_owned(),
            http,
            store: Arc::new(Mutex::new(store)),
            metadata: Arc::new(Mutex::new(None)),
            namespace_id,
        }
    }

    /// The base URL this client targets. Exposed primarily for
    /// diagnostics — `bitrouter cloud whoami` prints it.
    pub fn base_url(&self) -> &str {
        &self.base_url
    }

    /// The namespace this client's credential is baked into, or `None`
    /// for a namespace-null / pre-namespace credential. `bitrouter cloud
    /// whoami` prints it; `namespace list` marks the active one.
    pub fn namespace_id(&self) -> Option<&str> {
        self.namespace_id.as_deref()
    }

    /// Build a namespace-scoped path `/v1/namespaces/{nsid}{suffix}`,
    /// erroring with [`Error::NoNamespace`] when the credential carries
    /// no namespace. `suffix` must start with `/` (e.g. `/keys`,
    /// `/policies/effective`). Centralising the join keeps every
    /// namespace-scoped method from re-deriving the prefix and guards
    /// the "no namespace" case in exactly one place.
    pub(super) fn namespaced(&self, suffix: &str) -> Result<String> {
        let nsid = self.namespace_id.as_deref().ok_or(Error::NoNamespace)?;
        Ok(format!("/v1/namespaces/{nsid}{suffix}"))
    }

    /// Fetch a current bearer, refreshing if the stored access token
    /// is within
    /// [`crate::auth::credentials::REFRESH_WINDOW`] of expiry. The
    /// rotated refresh token (if any) is persisted before the bearer
    /// is returned.
    async fn bearer(&self) -> Result<String> {
        let mut store = self.store.lock().await;
        let creds = store
            .current()
            .ok_or(Error::NotSignedIn)?
            .authorization_server
            .clone();
        let metadata = self.resolve_metadata(&creds).await?;
        store
            .current_token(&self.http, &metadata)
            .await
            .map_err(Error::Auth)
    }

    async fn resolve_metadata(&self, as_url: &str) -> Result<AsMetadata> {
        let mut cell = self.metadata.lock().await;
        if let Some(cached) = cell.as_ref() {
            return Ok(cached.clone());
        }
        let fresh = metadata::fetch(&self.http, as_url)
            .await
            .with_context(|| format!("fetching AS metadata at {as_url}"))
            .map_err(Error::Auth)?;
        *cell = Some(fresh.clone());
        Ok(fresh)
    }

    /// Build a request with `Authorization: Bearer …` already attached.
    /// Per-method helpers layer the JSON body / query string on top.
    async fn authed(&self, method: Method, path: &str) -> Result<reqwest::RequestBuilder> {
        let bearer = self.bearer().await?;
        let url = format!("{}{}", self.base_url, path);
        let value = HeaderValue::from_str(&format!("Bearer {bearer}"))
            .context("encoding Authorization header")
            .map_err(Error::Auth)?;
        let mut headers = HeaderMap::new();
        headers.insert(AUTHORIZATION, value);
        Ok(self.http.request(method, url).headers(headers))
    }

    pub(super) async fn get_json<Resp: DeserializeOwned>(&self, path: &str) -> Result<Resp> {
        let req = self.authed(Method::GET, path).await?;
        execute_json(req).await
    }

    pub(super) async fn get_with_query<Q: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        query: &Q,
    ) -> Result<Resp> {
        let req = self.authed(Method::GET, path).await?.query(query);
        execute_json(req).await
    }

    pub(super) async fn post_json<Body: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        body: &Body,
    ) -> Result<Resp> {
        let req = self.authed(Method::POST, path).await?.json(body);
        execute_json(req).await
    }

    pub(super) async fn post_empty<Resp: DeserializeOwned>(&self, path: &str) -> Result<Resp> {
        let req = self.authed(Method::POST, path).await?;
        execute_json(req).await
    }

    pub(super) async fn put_json<Body: Serialize, Resp: DeserializeOwned>(
        &self,
        path: &str,
        body: &Body,
    ) -> Result<Resp> {
        let req = self.authed(Method::PUT, path).await?.json(body);
        execute_json(req).await
    }

    pub(super) async fn delete_json<Resp: DeserializeOwned>(&self, path: &str) -> Result<Resp> {
        let req = self.authed(Method::DELETE, path).await?;
        execute_json(req).await
    }
}

/// Reusable reqwest client with the SDK user-agent and a sensible
/// timeout. Centralised so every outgoing call sends a consistent UA
/// (RFC 9110 §10.1.5).
fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("bitrouter-cloud-sdk/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("building bitrouter-cloud-sdk HTTP client")
        .map_err(Error::Auth)
}

/// Drive a built request to completion, mapping a non-2xx response
/// onto the server's `{ error, error_description }` envelope and a 2xx
/// body onto `Resp`. Empty 2xx bodies (e.g. `204 No Content`) are
/// disallowed — every server endpoint in v1 returns a JSON body, so an
/// empty success is treated as a decode error.
async fn execute_json<Resp: DeserializeOwned>(req: reqwest::RequestBuilder) -> Result<Resp> {
    let resp = req.send().await?;
    let status = resp.status();
    let body_bytes = resp.bytes().await?;
    if status.is_success() {
        let parsed = serde_json::from_slice::<Resp>(&body_bytes)?;
        return Ok(parsed);
    }
    Err(map_error_response(status, &body_bytes))
}

fn map_error_response(status: StatusCode, body: &[u8]) -> Error {
    // Try the structured `{ error, error_description }` envelope first.
    if let Ok(envelope) = serde_json::from_slice::<error::ErrorBody>(body) {
        return envelope.into_error(status.as_u16());
    }
    // Fall back to whatever the body says (or the status reason when
    // the body is empty / non-UTF-8). 401 / 403 from an intermediary
    // (e.g. a CDN authn layer) will land here.
    let message = String::from_utf8_lossy(body).into_owned();
    let message = if message.trim().is_empty() {
        status
            .canonical_reason()
            .unwrap_or("unexpected status")
            .to_owned()
    } else {
        message
    };
    match status.as_u16() {
        400 => Error::BadRequest { message },
        401 => Error::Unauthorized { message },
        403 => Error::Forbidden {
            message,
            missing_scope: None,
        },
        404 => Error::NotFound { message },
        409 => Error::Conflict { message },
        s => Error::Server { status: s, message },
    }
}
