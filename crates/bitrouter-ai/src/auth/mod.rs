//! Explicit authentication mechanisms for a caller-selected provider/account.
//!
//! Registration performs no activation, account discovery or interactive login.
//! Implementations own any irreversible refresh through an injected store.

pub mod credentials;
pub mod device_code;
pub mod oauth;
pub mod store;

#[cfg(feature = "file-store")]
pub mod file;

#[cfg(feature = "pkce")]
pub mod auth_code;
#[cfg(feature = "pkce")]
pub mod listener;
#[cfg(feature = "pkce")]
pub mod login;
#[cfg(feature = "pkce")]
pub mod pkce;

use std::collections::HashMap;
use std::sync::Arc;

use async_trait::async_trait;
use sha2::{Digest, Sha256};

use crate::error::{ModelError, Result};
use crate::target::ModelTarget;
use crate::types::{ApiProtocol, AuthScheme};

const AUTHORITY_DOMAIN: &[u8] = b"bitrouter.transport.credential-authority.v1";

/// Authentication boundary at which opaque extension diagnostics are discarded.
#[derive(Clone, Copy)]
pub enum AuthOperation {
    /// Structured request-body preparation.
    BodyPreparation,
    /// Initial wire authentication.
    RequestAuthentication,
    /// Recovery after an upstream rejection.
    Refresh,
    /// Resolution of the selected credential principal.
    ContinuationAuthorityResolution,
}

impl AuthOperation {
    fn diagnostic(self) -> &'static str {
        match self {
            Self::BodyPreparation => "upstream authentication body preparation failed",
            Self::RequestAuthentication => "upstream authentication failed",
            Self::Refresh => "upstream authentication refresh failed",
            Self::ContinuationAuthorityResolution => {
                "continuation authentication authority resolution failed"
            }
        }
    }
}

/// Keep domain failure facts while discarding extension-controlled secret text.
pub fn normalize_auth_extension_error(error: ModelError, operation: AuthOperation) -> ModelError {
    let message = operation.diagnostic().to_owned();
    match error {
        ModelError::InvalidRequest { .. } => ModelError::InvalidRequest { message },
        ModelError::InvalidResponse { .. } => ModelError::InvalidResponse {
            message,
            usage: None,
        },
        ModelError::DecisionResponse { .. } => ModelError::InvalidResponse {
            message,
            usage: None,
        },
        ModelError::Provider { status, .. } => ModelError::Provider { status, message },
        ModelError::PolicyViolation { .. } => ModelError::PolicyViolation { message },
        ModelError::InvalidCredential { .. } => ModelError::InvalidCredential { message },
        ModelError::Configuration { .. } => ModelError::Configuration { message },
        ModelError::Transport { .. } => ModelError::Transport { message },
        ModelError::Decode { .. } => ModelError::Decode { message },
        ModelError::HttpResponse {
            status,
            retry_after,
            ..
        } => ModelError::HttpResponse {
            status,
            body: message,
            retry_after,
        },
        error @ (ModelError::Incompatible { .. }
        | ModelError::Timeout
        | ModelError::Cancelled
        | ModelError::CredentialStorage { .. }) => error,
    }
}

/// Redaction-safe stable identity for the credential principal used on one
/// outbound request. The value is already a one-way digest; raw credentials
/// and account identifiers never enter pipeline events, logs, or storage.
#[derive(Clone, PartialEq, Eq, Hash)]
pub struct CredentialAuthority([u8; 32]);

impl CredentialAuthority {
    /// Derive an authority proof from a provider-scoped stable identity.
    ///
    /// `identity` may be a stable account/subject id or, when no stable
    /// principal exists, the long-lived stored credential itself. Callers must
    /// never log the input or retain it beyond this constructor.
    pub fn derive(namespace: &str, identity: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(AUTHORITY_DOMAIN);
        digest.update((namespace.len() as u64).to_be_bytes());
        digest.update(namespace.as_bytes());
        digest.update((identity.len() as u64).to_be_bytes());
        digest.update(identity.as_bytes());
        Self(digest.finalize().into())
    }

    /// Derive a proof from an additional provider-controlled identity scope,
    /// such as an OAuth issuer. Every component is length-delimited so
    /// distinct `(scope, identity)` pairs cannot collide by concatenation.
    pub fn derive_scoped(namespace: &str, scope: &str, identity: &str) -> Self {
        let mut digest = Sha256::new();
        digest.update(AUTHORITY_DOMAIN);
        for component in [namespace, scope, identity] {
            digest.update((component.len() as u64).to_be_bytes());
            digest.update(component.as_bytes());
        }
        Self(digest.finalize().into())
    }

    /// Digest bytes for a second, installation-keyed fingerprinting layer.
    pub fn proof_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

impl std::fmt::Debug for CredentialAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("CredentialAuthority(<redacted>)")
    }
}

/// Stable continuation authority proven for one exact authenticated request:
/// both the credential principal and the scheme actually installed on wire.
#[derive(Clone, PartialEq, Eq)]
pub struct ContinuationAuthority {
    credential: CredentialAuthority,
    effective_scheme: AuthScheme,
}

impl ContinuationAuthority {
    /// Combine a stable credential principal with the scheme used on wire.
    pub fn new(credential: CredentialAuthority, effective_scheme: AuthScheme) -> Self {
        Self {
            credential,
            effective_scheme,
        }
    }

    /// Return the redaction-safe credential-principal proof.
    pub fn credential(&self) -> &CredentialAuthority {
        &self.credential
    }

    /// Return the authentication scheme actually installed on the request.
    pub fn effective_scheme(&self) -> AuthScheme {
        self.effective_scheme
    }

    /// Verify that the request still has the proven authentication scheme.
    pub fn validates_final_request(&self, request: &reqwest::Request) -> bool {
        request_effective_auth_scheme(request) == Some(self.effective_scheme)
    }

    /// Bind an explicitly resolved set of account-selection headers.
    pub fn with_request_scope(mut self, headers: &reqwest::header::HeaderMap) -> Self {
        if headers.is_empty() {
            return self;
        }
        let mut digest = Sha256::new();
        digest.update(b"bitrouter.transport.request-scope.v1");
        digest.update(self.credential.proof_bytes());
        for name in CONTINUATION_SCOPE_HEADERS {
            digest_header(&mut digest, headers, name);
        }
        self.credential = CredentialAuthority(digest.finalize().into());
        self
    }
}

const CONTINUATION_SCOPE_HEADERS: [&str; 5] = [
    "openai-organization",
    "openai-project",
    "anthropic-workspace-id",
    "chatgpt-account-id",
    "x-goog-user-project",
];

/// Whether a header selects an account or billing scope.
pub fn is_continuation_scope_header(name: &str) -> bool {
    CONTINUATION_SCOPE_HEADERS.contains(&name)
}

fn digest_header(digest: &mut Sha256, headers: &reqwest::header::HeaderMap, name: &str) {
    digest.update((name.len() as u64).to_be_bytes());
    digest.update(name.as_bytes());
    let values = headers.get_all(name);
    digest.update((values.iter().count() as u64).to_be_bytes());
    for value in values {
        digest.update((value.as_bytes().len() as u64).to_be_bytes());
        digest.update(value.as_bytes());
    }
}

impl std::fmt::Debug for ContinuationAuthority {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ContinuationAuthority")
            .field("credential", &self.credential)
            .field("effective_scheme", &self.effective_scheme)
            .finish()
    }
}

fn static_effective_auth_scheme(target: &ModelTarget) -> AuthScheme {
    match target.api_protocol {
        ApiProtocol::ChatCompletions | ApiProtocol::Responses | ApiProtocol::Decisions => {
            AuthScheme::Bearer
        }
        ApiProtocol::Messages => target.auth_scheme,

        ApiProtocol::Custom(_) => target.auth_scheme,
    }
}

fn request_effective_auth_scheme(request: &reqwest::Request) -> Option<AuthScheme> {
    let authorization = request
        .headers()
        .get_all(reqwest::header::AUTHORIZATION)
        .iter()
        .collect::<Vec<_>>();
    let x_keys = request
        .headers()
        .get_all("x-api-key")
        .iter()
        .chain(request.headers().get_all("x-goog-api-key").iter())
        .collect::<Vec<_>>();

    match (authorization.as_slice(), x_keys.as_slice()) {
        ([value], []) => {
            let value = value.to_str().ok()?;
            let mut fields = value.split_ascii_whitespace();
            let scheme = fields.next()?;
            let credential = fields.next()?;
            (scheme.eq_ignore_ascii_case("bearer")
                && !credential.is_empty()
                && fields.next().is_none())
            .then_some(AuthScheme::Bearer)
        }
        ([], [value]) => value
            .to_str()
            .ok()
            .filter(|credential| !credential.trim().is_empty())
            .map(|_| AuthScheme::XApiKey),
        _ => None,
    }
}

/// Result of applying authentication to one exact outbound request.
///
/// Dynamic appliers return the stable authority proof atomically with the
/// request whose transport credential they just installed. `None` means the
/// applier cannot prove a stable continuation authority; ordinary requests
/// still work, but successful native Responses continuation publication fails
/// closed.
///
/// Construct the proof after installing authentication. Later mutation of the
/// destination, credential values or known account-selection headers invalidates
/// the proof. Trace and ordinary compatibility headers may still be attached.
pub struct AppliedAuth {
    request: reqwest::Request,
    continuation_authority: Option<ContinuationAuthority>,
    // Transient commitment to this exact authentication, separate from stable
    // principal identity so an OAuth refresh can retain the same principal.
    wire_authentication: Option<[u8; 32]>,
}

impl AppliedAuth {
    /// Build an authenticated request with a proven stable authority.
    pub fn proven(request: reqwest::Request, authority: CredentialAuthority) -> Self {
        let continuation_authority = request_effective_auth_scheme(&request)
            .map(|scheme| ContinuationAuthority::new(authority, scheme));
        Self {
            wire_authentication: continuation_authority
                .as_ref()
                .map(|_| wire_authentication_digest(&request)),
            request,
            continuation_authority,
        }
    }

    /// Build a proof with an explicitly reported effective wire scheme.
    pub fn proven_with_scheme(
        request: reqwest::Request,
        authority: CredentialAuthority,
        effective_scheme: AuthScheme,
    ) -> Self {
        let continuation_authority = (request_effective_auth_scheme(&request)
            == Some(effective_scheme))
        .then(|| ContinuationAuthority::new(authority, effective_scheme));
        Self {
            wire_authentication: continuation_authority
                .as_ref()
                .map(|_| wire_authentication_digest(&request)),
            request,
            continuation_authority,
        }
    }

    /// Build an authenticated request whose applier cannot prove a stable
    /// continuation authority.
    pub fn unproven(request: reqwest::Request) -> Self {
        Self {
            request,
            continuation_authority: None,
            wire_authentication: None,
        }
    }

    /// Discard continuation proof metadata and recover the authenticated request.
    pub fn into_request(self) -> reqwest::Request {
        self.request
    }

    /// Recover the request and authority after checking final wire mutations.
    pub fn into_parts(self) -> (reqwest::Request, Option<ContinuationAuthority>) {
        let authority = self.continuation_authority.filter(|authority| {
            request_effective_auth_scheme(&self.request) == Some(authority.effective_scheme)
                && self.wire_authentication == Some(wire_authentication_digest(&self.request))
        });
        (self.request, authority)
    }
}

/// Bind the credential values, destination and known account-selection headers
/// observed at authentication time. Framing, tracing and compatibility headers
/// may still change without invalidating that proof. Never publish this digest.
fn wire_authentication_digest(request: &reqwest::Request) -> [u8; 32] {
    let mut digest = Sha256::new();
    digest.update(b"bitrouter.transport.wire-authentication.v1");
    let url = request.url().as_str();
    digest.update((url.len() as u64).to_be_bytes());
    digest.update(url.as_bytes());
    for name in [
        "authorization",
        "x-api-key",
        "x-goog-api-key",
        "openai-organization",
        "openai-project",
        "anthropic-workspace-id",
        "chatgpt-account-id",
        "x-goog-user-project",
    ] {
        digest_header(&mut digest, request.headers(), name);
    }
    digest.finalize().into()
}

impl std::ops::Deref for AppliedAuth {
    type Target = reqwest::Request;

    fn deref(&self) -> &Self::Target {
        &self.request
    }
}

impl std::ops::DerefMut for AppliedAuth {
    fn deref_mut(&mut self) -> &mut Self::Target {
        &mut self.request
    }
}

impl std::fmt::Debug for AppliedAuth {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("AppliedAuth")
            .field("method", self.request.method())
            .field("url", &"<redacted>")
            .field(
                "continuation_authority_proven",
                &self.continuation_authority.is_some(),
            )
            .finish()
    }
}

/// Apply provider-specific authentication to a built `reqwest::Request`.
///
/// Receives ownership of the request and the resolved [`ModelTarget`];
/// returns the request with credentials + any required integration headers
/// added. May perform async work (token-store reads, network token
/// exchanges) — the executor awaits the result before sending.
#[async_trait]
pub trait AuthApplier: Send + Sync {
    /// Whether provider body/auth shaping preserves a requested output-token
    /// limit. Unknown remains `None`; managed execution also checks the final
    /// wire body before sending it.
    fn output_token_limit_support(&self, _target: &ModelTarget) -> Option<bool> {
        None
    }

    /// Pure, deterministic normalization of the expected managed wire body.
    /// This must preserve required input and controls. It performs no I/O and
    /// must not depend on credentials; final authenticated requests are checked
    /// against this baseline. The default expects unchanged semantic fields.
    fn normalize_managed_body(
        &self,
        _body: &mut serde_json::Value,
        _target: &ModelTarget,
    ) -> Result<()> {
        Ok(())
    }

    /// Apply authentication. The default `Transport::authorise` is **not**
    /// called when this applier runs; the applier owns the full credential
    /// surface for the request.
    async fn apply(
        &self,
        request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<reqwest::Request>;

    /// Apply authentication and atomically report the stable continuation
    /// authority used by that exact request.
    ///
    /// The default delegates to [`apply`](Self::apply) and returns an unproven
    /// authority. Native continuation requires an explicit stable proof.
    async fn apply_with_authority(
        &self,
        request: reqwest::Request,
        target: &ModelTarget,
    ) -> Result<AppliedAuth> {
        Ok(AppliedAuth::unproven(self.apply(request, target).await?))
    }

    /// Resolve the stable authority that a newly authenticated request would
    /// use. Continuation route matching calls this before upstream dispatch.
    /// The later [`apply_with_authority`](Self::apply_with_authority) result
    /// remains authoritative for the exact request and is checked against this
    /// proof before send.
    async fn continuation_authority(
        &self,
        _target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        Ok(None)
    }

    /// Resolve the route-time principal + effective wire scheme proof. The
    /// apply-time [`AppliedAuth`] proof is compared against this exact value
    /// before dispatch, closing both credential and auth-scheme races.
    async fn continuation_authority_proof(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<ContinuationAuthority>> {
        Ok(self
            .continuation_authority(target)
            .await?
            .map(|credential| {
                ContinuationAuthority::new(credential, static_effective_auth_scheme(target))
            }))
    }

    /// Optionally rewrite the structured request body before it is
    /// serialized and sent. Runs at render time — after the protocol
    /// adapter produces the JSON body and before the HTTP request is built,
    /// so it sees the body as a mutable [`serde_json::Value`]. This is the
    /// right layer for body edits; [`apply`](Self::apply) only sees an
    /// already-built request whose body is opaque bytes.
    ///
    /// The default is a no-op. OAuth *subscription* providers override it to
    /// match the body shape the vendor's own first-party client sends — for
    /// example Claude Pro/Max requires Claude Code's identity as the first
    /// `system` block, and the ChatGPT/Codex backend requires `store: false`
    /// on the Responses body. Static-credential providers never need it.
    async fn prepare_body(
        &self,
        _body: &mut serde_json::Value,
        _target: &ModelTarget,
    ) -> Result<()> {
        Ok(())
    }

    /// Give stateful auth providers one chance to recover from an upstream
    /// `401 Unauthorized`.
    ///
    /// The executor calls this only after the upstream rejects an already
    /// authenticated request. Implementations should refresh or reload their
    /// credential state, then return `true` when the request should be rebuilt
    /// and retried once. Static-credential providers keep the default `false`
    /// and preserve the original upstream error.
    async fn refresh_after_unauthorized(
        &self,
        _target: &ModelTarget,
        _rejected_authorization: Option<&reqwest::header::HeaderValue>,
    ) -> Result<bool> {
        Ok(false)
    }
}

/// Registry of per-provider [`AuthApplier`]s, keyed by `provider_name`.
///
/// Empty by default — the executor falls through to `Transport::authorise`
/// for any provider with no registered applier, which is the right path for
/// static-credential providers.
#[derive(Default, Clone)]
pub struct AuthAppliers {
    by_provider: HashMap<String, Arc<dyn AuthApplier>>,
}

impl AuthAppliers {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `applier` for requests whose `target.provider_name == provider_id`.
    /// Re-registering overwrites the previous entry.
    pub fn register(&mut self, provider_id: impl Into<String>, applier: Arc<dyn AuthApplier>) {
        self.by_provider.insert(provider_id.into(), applier);
    }

    /// Chained-builder form of [`register`](Self::register).
    pub fn with(mut self, provider_id: impl Into<String>, applier: Arc<dyn AuthApplier>) -> Self {
        self.register(provider_id, applier);
        self
    }

    /// Look up an applier for `provider_id`.
    pub fn lookup(&self, provider_id: &str) -> Option<&Arc<dyn AuthApplier>> {
        self.by_provider.get(provider_id)
    }

    /// Whether any appliers are registered.
    pub fn is_empty(&self) -> bool {
        self.by_provider.is_empty()
    }

    /// Resolve a target's stable continuation authority. Registered dynamic
    /// appliers own the proof; static transport auth derives it from the
    /// effective configured credential without retaining the secret.
    pub async fn continuation_authority(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<CredentialAuthority>> {
        if let Some(applier) = self.lookup(&target.provider_name) {
            return applier
                .continuation_authority(target)
                .await
                .map_err(|error| {
                    normalize_auth_extension_error(
                        error,
                        AuthOperation::ContinuationAuthorityResolution,
                    )
                });
        }
        let credential = target.api_key.as_str();
        Ok(Some(CredentialAuthority::derive(
            "static-transport-credential",
            credential,
        )))
    }

    /// Resolve the selected credential principal and effective wire scheme.
    pub async fn continuation_authority_proof(
        &self,
        target: &ModelTarget,
    ) -> Result<Option<ContinuationAuthority>> {
        if let Some(applier) = self.lookup(&target.provider_name) {
            return applier
                .continuation_authority_proof(target)
                .await
                .map_err(|error| {
                    normalize_auth_extension_error(
                        error,
                        AuthOperation::ContinuationAuthorityResolution,
                    )
                });
        }
        let credential = target.api_key.as_str();
        Ok(Some(ContinuationAuthority::new(
            CredentialAuthority::derive("static-transport-credential", credential),
            static_effective_auth_scheme(target),
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::ApiProtocol;

    #[test]
    fn applied_authority_rejects_mutated_wire_credentials() -> Result<()> {
        for (name, before, after) in [
            ("authorization", "Bearer first", "Bearer second"),
            ("x-api-key", "first", "second"),
            ("x-goog-api-key", "first", "second"),
        ] {
            let request = reqwest::Client::new()
                .post("https://example.invalid/v1/generate")
                .header(name, before)
                .build()
                .map_err(|_| ModelError::invalid_request("fixture request failed"))?;
            let mut applied = AppliedAuth::proven(
                request,
                CredentialAuthority::derive("test", "same-principal"),
            );
            applied.headers_mut().insert(
                reqwest::header::HeaderName::from_static(name),
                reqwest::header::HeaderValue::from_static(after),
            );
            let (request, authority) = applied.into_parts();
            assert!(request_effective_auth_scheme(&request).is_some());
            assert!(authority.is_none(), "mutated {name} retained old proof");
        }
        Ok(())
    }

    #[test]
    fn applied_authority_rejects_mutated_destination_and_account_scope() -> Result<()> {
        for name in [
            "openai-organization",
            "openai-project",
            "anthropic-workspace-id",
            "chatgpt-account-id",
            "x-goog-user-project",
            "destination",
            "credential-family",
        ] {
            let request = reqwest::Client::new()
                .post("https://example.invalid/v1/generate")
                .header("x-api-key", "first")
                .build()
                .map_err(|_| ModelError::invalid_request("fixture request failed"))?;
            let mut applied = AppliedAuth::proven(
                request,
                CredentialAuthority::derive("test", "same-principal"),
            );
            match name {
                "destination" => applied.url_mut().set_path("/another-account/generate"),
                "credential-family" => {
                    applied.headers_mut().remove("x-api-key");
                    applied.headers_mut().insert(
                        "x-goog-api-key",
                        reqwest::header::HeaderValue::from_static("first"),
                    );
                }
                _ => {
                    applied.headers_mut().insert(
                        reqwest::header::HeaderName::from_static(name),
                        reqwest::header::HeaderValue::from_static("other-account"),
                    );
                }
            }
            assert!(applied.into_parts().1.is_none(), "mutation: {name}");
        }
        Ok(())
    }

    #[test]
    fn applied_authority_rejects_replaced_removed_or_duplicate_scope() -> Result<()> {
        for scope in ["openai-organization", "openai-project"] {
            for mutation in ["replace", "remove", "duplicate"] {
                let request = reqwest::Client::new()
                    .post("https://example.invalid/v1/generate")
                    .header("authorization", "Bearer first")
                    .header(scope, "initial-scope")
                    .build()
                    .map_err(|_| ModelError::invalid_request("fixture request failed"))?;
                let mut applied = AppliedAuth::proven(
                    request,
                    CredentialAuthority::derive("test", "same-principal"),
                );
                let header = reqwest::header::HeaderName::from_static(scope);
                let value = reqwest::header::HeaderValue::from_static("other-scope");
                match mutation {
                    "replace" => {
                        applied.headers_mut().insert(header, value);
                    }
                    "remove" => {
                        applied.headers_mut().remove(header);
                    }
                    _ => {
                        applied.headers_mut().append(header, value);
                    }
                }
                assert!(applied.into_parts().1.is_none(), "{mutation}: {scope}");
            }
        }
        Ok(())
    }

    #[test]
    fn atomic_auth_refresh_retains_principal_and_allows_trace_headers() -> Result<()> {
        let principal = CredentialAuthority::derive("test", "same-principal");
        let expected = ContinuationAuthority::new(principal.clone(), AuthScheme::Bearer);
        for bearer in ["Bearer before-refresh", "Bearer after-refresh"] {
            let request = reqwest::Client::new()
                .post("https://example.invalid/v1/generate")
                .header("authorization", bearer)
                .build()
                .map_err(|_| ModelError::invalid_request("fixture request failed"))?;
            let mut applied = AppliedAuth::proven(request, principal.clone());
            for name in ["traceparent", "x-bitrouter-request-id", "anthropic-beta"] {
                applied.headers_mut().insert(
                    reqwest::header::HeaderName::from_static(name),
                    reqwest::header::HeaderValue::from_static("fixture-value"),
                );
            }
            assert_eq!(applied.into_parts().1, Some(expected.clone()));
        }
        Ok(())
    }

    struct LegacyApplier;

    #[async_trait]
    impl AuthApplier for LegacyApplier {
        async fn apply(
            &self,
            mut request: reqwest::Request,
            _target: &ModelTarget,
        ) -> Result<reqwest::Request> {
            request.headers_mut().insert(
                reqwest::header::AUTHORIZATION,
                reqwest::header::HeaderValue::from_static("Bearer legacy"),
            );
            Ok(request)
        }
    }

    fn target() -> ModelTarget {
        ModelTarget {
            provider_name: "legacy-provider".into(),
            service_id: "legacy-model".into(),
            api_base: "https://example.invalid".into(),
            api_key: String::new(),
            api_protocol: ApiProtocol::ChatCompletions,
            account_label: None,
            credential_priority: Default::default(),
            compatibility: Default::default(),
            auth_scheme: Default::default(),
        }
    }

    #[tokio::test]
    async fn legacy_applier_remains_compatible_and_is_conservatively_unproven() {
        let request = reqwest::Client::new()
            .post("https://example.invalid")
            .build()
            .unwrap();

        let applied = LegacyApplier
            .apply_with_authority(request, &target())
            .await
            .unwrap();

        assert_eq!(
            applied.request.headers()[reqwest::header::AUTHORIZATION],
            "Bearer legacy"
        );
        assert!(applied.continuation_authority.is_none());
    }

    #[test]
    fn applied_auth_debug_redacts_the_complete_authenticated_url() {
        let request = reqwest::Client::new()
            .post(
                "https://debug-user:debug-password@example.invalid/private-path?api_key=debug-query-secret",
            )
            .build()
            .unwrap();
        let debug = format!("{:?}", AppliedAuth::unproven(request));

        for private in [
            "debug-user",
            "debug-password",
            "private-path",
            "api_key",
            "debug-query-secret",
        ] {
            assert!(
                !debug.contains(private),
                "AppliedAuth Debug exposed authenticated URL data: {debug}"
            );
        }
        assert!(debug.contains("<redacted>"));
    }

    #[test]
    fn opaque_error_normalization_retains_failure_class_without_private_detail() {
        const SENTINEL: &str = "auth-normalizer-private-sentinel";
        for error in [
            ModelError::InvalidRequest {
                message: SENTINEL.into(),
            },
            ModelError::InvalidResponse {
                message: SENTINEL.into(),
                usage: None,
            },
            ModelError::Provider {
                status: 503,
                message: SENTINEL.into(),
            },
            ModelError::Transport {
                message: SENTINEL.into(),
            },
        ] {
            let kind = std::mem::discriminant(&error);
            let normalized =
                normalize_auth_extension_error(error, AuthOperation::RequestAuthentication);
            assert_eq!(std::mem::discriminant(&normalized), kind);
            assert!(!normalized.to_string().contains(SENTINEL));
            assert!(!format!("{normalized:?}").contains(SENTINEL));
            assert!(std::error::Error::source(&normalized).is_none());
        }
    }

    #[test]
    fn effective_scheme_requires_one_well_formed_credential_header() {
        let client = reqwest::Client::new();
        let request = |authorization: Option<reqwest::header::HeaderValue>,
                       x_key: Option<reqwest::header::HeaderValue>| {
            let mut request = client.post("https://example.invalid").build().unwrap();
            if let Some(authorization) = authorization {
                request
                    .headers_mut()
                    .insert(reqwest::header::AUTHORIZATION, authorization);
            }
            if let Some(x_key) = x_key {
                request.headers_mut().insert("x-api-key", x_key);
            }
            request
        };

        let header = reqwest::header::HeaderValue::from_static;
        assert_eq!(
            request_effective_auth_scheme(&request(Some(header("Bearer secret")), None)),
            Some(AuthScheme::Bearer)
        );
        assert_eq!(
            request_effective_auth_scheme(&request(Some(header("bEaReR secret")), None)),
            Some(AuthScheme::Bearer)
        );
        assert_eq!(
            request_effective_auth_scheme(&request(None, Some(header("secret")))),
            Some(AuthScheme::XApiKey)
        );
        for invalid in [
            header("Basic secret"),
            header("AWS4-HMAC-SHA256 credential"),
            header("Bearer"),
            header("Bearer "),
            header("Bearer    "),
        ] {
            assert_eq!(
                request_effective_auth_scheme(&request(Some(invalid), None)),
                None
            );
        }
        assert_eq!(
            request_effective_auth_scheme(&request(
                Some(reqwest::header::HeaderValue::from_bytes(b"Bearer \xff").unwrap()),
                None,
            )),
            None
        );
        assert_eq!(
            request_effective_auth_scheme(&request(None, Some(header("")))),
            None
        );
        assert_eq!(
            request_effective_auth_scheme(&request(
                Some(header("Bearer secret")),
                Some(header("secret")),
            )),
            None
        );
        assert_eq!(request_effective_auth_scheme(&request(None, None)), None);
        let mismatched = AppliedAuth::proven_with_scheme(
            request(Some(header("Bearer secret")), None),
            CredentialAuthority::derive("test", "principal"),
            AuthScheme::XApiKey,
        );
        assert!(mismatched.continuation_authority.is_none());
    }
}
