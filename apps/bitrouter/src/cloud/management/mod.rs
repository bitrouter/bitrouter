//! Typed HTTP client for the BitRouter Cloud `/v1/*` management surface.
//!
//! The surface — `keys`, `usage`, `billing`, `policies`, `budgets`,
//! `presets`, `byok`, `oauth_clients` — is the same one the web console
//! consumes. It accepts either a `brk_` API key or an OAuth access token.
//! The client resolves a fresh, origin-confined bearer from the shared hosted
//! account manager for each request.
//!
//! ## Namespace scoping
//!
//! The server bifurcates the management surface: namespace-scoped endpoints
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

use std::sync::Arc;
use std::time::Duration;

use anyhow::Context;
use bitrouter_providers::hosted::account::credentials::CredentialKind;
use bitrouter_providers::hosted::account::manager::CredentialManager;
use reqwest::header::{AUTHORIZATION, HeaderMap, HeaderValue};
use reqwest::{Method, StatusCode};
use serde::Serialize;
use serde::de::DeserializeOwned;

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

use error::Error;

/// Convenience `Result` alias used by every method on
/// [`ManagementClient`].
pub type Result<T> = std::result::Result<T, Error>;

/// Typed client for the BitRouter Cloud `/v1/*` management surface.
///
/// Construct via [`ManagementClient::from_manager`] and call the per-method
/// helpers defined in this module's submodules.
pub struct ManagementClient {
    /// Cloud base URL (no trailing slash). The credentials file's
    /// `authorization_server` field is the source of truth — `bitrouter
    /// auth login` against `https://my-self-hosted.example.com` makes
    /// this client target that same host.
    base_url: String,
    http: reqwest::Client,
    /// Shared manager that serializes refresh and persistence.
    manager: Arc<CredentialManager>,
    /// The namespace the stored credential is baked into, captured at
    /// construction. `Some` for every device-flow token; `None` only
    /// for a namespace-null credential or a pre-namespace credential
    /// file. Namespace-scoped methods resolve the `{nsid}` path segment
    /// from this via [`ManagementClient::namespaced`]. Immutable for the
    /// client's lifetime — refresh never rebinds the namespace.
    namespace_id: Option<String>,
    /// Authentication mechanism used to resolve bearer and namespace paths.
    credential_kind: CredentialKind,
}

impl std::fmt::Debug for ManagementClient {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ManagementClient")
            .field("base_url", &self.base_url)
            .field("namespace_id", &self.namespace_id)
            .field("credential_kind", &self.credential_kind)
            .field("manager", &"<redacted>")
            .finish_non_exhaustive()
    }
}

impl ManagementClient {
    /// Build a client from the current hosted account credential.
    pub async fn from_manager(manager: Arc<CredentialManager>) -> Result<Self> {
        let creds = manager
            .current()
            .await
            .context("reading BitRouter Cloud credentials")
            .map_err(Error::Auth)?;
        let creds = creds.ok_or(Error::NotSignedIn)?;
        let base_url = creds.base_url().trim_end_matches('/').to_owned();
        let namespace_id = creds.namespace_id().map(ToOwned::to_owned);
        let credential_kind = creds.kind();
        let http = build_http_client()?;
        Ok(Self {
            base_url,
            http,
            manager,
            namespace_id,
            credential_kind,
        })
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
        let nsid = match (self.namespace_id.as_deref(), self.credential_kind) {
            (Some(namespace_id), _) => namespace_id,
            (None, CredentialKind::ApiKey) => "me",
            (None, CredentialKind::Oauth) => return Err(Error::NoNamespace),
        };
        Ok(format!("/v1/namespaces/{nsid}{suffix}"))
    }

    /// Fetch a fresh bearer confined to the management client's origin.
    async fn bearer(&self) -> Result<String> {
        self.manager
            .resolve_bearer(None, Some(&self.base_url))
            .await
            .map(|credential| credential.secret().to_owned())
            .map_err(|error| Error::Auth(anyhow::anyhow!(error)))
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

/// Reusable reqwest client with the application user-agent and a sensible
/// timeout. Centralised so every outgoing call sends a consistent UA
/// (RFC 9110 §10.1.5).
fn build_http_client() -> Result<reqwest::Client> {
    reqwest::Client::builder()
        .user_agent(concat!("bitrouter/", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("building BitRouter Cloud management HTTP client")
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
