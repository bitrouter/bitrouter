//! JWT and SIWG authentication filters for the bitrouter gateway.
//!
//! Implements self-signed EdDSA (Ed25519) JWT authentication and native SIWG
//! (Sign-In with Google/Wallet) embedded wallet authentication:
//!
//! - **JWT path**: The JWT `iss` claim carries the signer's CAIP-10 identity.
//!   The server verifies the Ed25519/EIP-191 signature before any DB interaction.
//! - **SIWG path**: A SIWG credential is a signed challenge+nonce+timestamp.
//!   The server verifies the signature, extracts the CAIP-10 identity, and
//!   resolves an account — same as the JWT path.
//!
//! Both methods are accepted for all routes, including admin commands.
//! On first contact the server auto-creates an account for the public key.
//!
//! Credentials are extracted from the protocol-appropriate header:
//!
//! | Protocol   | Header                          |
//! |------------|---------------------------------|
//! | OpenAI     | `Authorization: Bearer <token>` |
//! | Anthropic  | `x-api-key: <token>`            |
//! | Management | `Authorization: Bearer <token>` |
//!
//! When no database is configured, auth is disabled and all requests are
//! allowed through (open proxy mode).

use std::sync::Arc;

use base64::Engine;
use base64::engine::general_purpose::URL_SAFE_NO_PAD;
use sea_orm::DatabaseConnection;
use warp::Filter;

use bitrouter_accounts::identity::{AccountId, Identity, Scope};
use bitrouter_accounts::service::AccountService;
use bitrouter_core::auth::chain::{Caip10, JwtAlgorithm};
use bitrouter_core::auth::claims::TokenScope;
use bitrouter_core::auth::token as jwt_token;

/// Shared auth state passed into filters.
#[derive(Clone)]
pub struct JwtAuthContext {
    /// Database connection for account lookups (auto-creation).
    db: Option<DatabaseConnection>,
}

impl JwtAuthContext {
    pub fn new(db: Option<DatabaseConnection>) -> Self {
        Self { db }
    }

    /// Returns `true` when no database is configured (open proxy mode).
    pub fn is_open(&self) -> bool {
        self.db.is_none()
    }
}

// ── credential extraction ─────────────────────────────────────

/// Extract a bearer token from `Authorization: Bearer <token>`.
fn extract_bearer(header: &str) -> Option<&str> {
    header
        .strip_prefix("Bearer ")
        .or_else(|| header.strip_prefix("bearer "))
}

/// Warp filter: extract credential from `Authorization: Bearer` header.
pub fn bearer_credential() -> impl Filter<Extract = (String,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization").and_then(
        |header: Option<String>| async move {
            match header.and_then(|h| extract_bearer(&h).map(str::to_owned)) {
                Some(key) => Ok(key),
                None => Err(warp::reject::custom(Unauthorized("missing bearer token"))),
            }
        },
    )
}

/// Warp filter: extract credential from `x-api-key` header.
pub fn x_api_key_credential() -> impl Filter<Extract = (String,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("x-api-key").and_then(|header: Option<String>| async move {
        match header {
            Some(key) if !key.is_empty() => Ok(key),
            _ => Err(warp::reject::custom(Unauthorized("missing x-api-key"))),
        }
    })
}

/// Warp filter: extract credential from either `Authorization: Bearer` **or**
/// `x-api-key` (Anthropic-style). Bearer takes precedence.
pub fn any_credential() -> impl Filter<Extract = (String,), Error = warp::Rejection> + Clone {
    warp::header::optional::<String>("authorization")
        .and(warp::header::optional::<String>("x-api-key"))
        .and_then(
            |auth_header: Option<String>, x_api_key: Option<String>| async move {
                if let Some(key) = auth_header.and_then(|h| extract_bearer(&h).map(str::to_owned)) {
                    return Ok(key);
                }
                if let Some(key) = x_api_key.filter(|k| !k.is_empty()) {
                    return Ok(key);
                }
                Err(warp::reject::custom(Unauthorized(
                    "missing authentication credentials",
                )))
            },
        )
}

// ── identity resolution ───────────────────────────────────────

/// Resolve a credential string to an [`Identity`].
///
/// Tries JWT first (the common case), then SIWG if JWT verification fails.
/// Both paths resolve to the same account model via CAIP-10 identity.
///
/// Security-critical ordering (per method):
/// 1. Verify signature cryptographically.
/// 2. Check expiration / replay protection.
/// 3. Look up / auto-create account in DB using CAIP-10 identity.
/// 4. Build Identity with account-relative scope.
async fn resolve_identity(
    credential: &str,
    ctx: &JwtAuthContext,
) -> Result<Identity, warp::Rejection> {
    // Try JWT first (most common path).
    if let Ok(identity) = resolve_jwt_identity(credential, ctx).await {
        return Ok(identity);
    }

    // Try SIWG if JWT didn't work.
    if let Ok(identity) = resolve_siwg_identity(credential, ctx).await {
        return Ok(identity);
    }

    Err(warp::reject::custom(Unauthorized(
        "invalid credential — neither valid JWT nor SIWG",
    )))
}

/// Resolve a JWT credential to an [`Identity`].
async fn resolve_jwt_identity(
    credential: &str,
    ctx: &JwtAuthContext,
) -> Result<Identity, warp::Rejection> {
    // 1. Verify signature (detects algorithm from header, verifies against iss).
    let claims = jwt_token::verify(credential)
        .map_err(|_| warp::reject::custom(Unauthorized("invalid JWT signature")))?;

    // 2. Check expiration.
    jwt_token::check_expiration(&claims)
        .map_err(|_| warp::reject::custom(Unauthorized("JWT expired")))?;

    // 3. Resolve account from DB using CAIP-10 iss.
    let Some(ref db) = ctx.db else {
        return Err(warp::reject::custom(Unauthorized(
            "authentication requires a database",
        )));
    };

    let svc = AccountService::new(db);
    let account = svc
        .find_or_create_by_pubkey(&claims.iss)
        .await
        .map_err(|_| warp::reject::custom(Unauthorized("account lookup failed")))?;

    let Some(account) = account else {
        return Err(warp::reject::custom(Unauthorized(
            "public key has been rotated — generate a new token with your current key",
        )));
    };

    // 4. Build identity with account-relative scope.
    let scope = match claims.scope {
        TokenScope::Admin => Scope::Admin,
        TokenScope::Api => Scope::Api,
    };

    Ok(Identity {
        account_id: AccountId(account.id),
        scope,
    })
}

// ── SIWG (Sign-In With G) authentication ──────────────────────

/// SIWG credential prefix used to distinguish from JWT tokens.
const SIWG_CREDENTIAL_PREFIX: &str = "siwg:";

/// Maximum age of a SIWG credential before it's considered stale (seconds).
const SIWG_MAX_AGE_SECS: u64 = 300;

/// Resolve a SIWG credential to an [`Identity`].
///
/// SIWG credentials are prefixed with `siwg:` followed by a base64url-encoded
/// JSON blob:
///
/// ```json
/// {
///   "iss": "solana:5eykt4UsFv8P8NJdTREpY1vzqKqZKvdp:<base58_pubkey>",
///   "nonce": "<hex_string>",
///   "iat": <unix_timestamp>,
///   "sig": "<base64url_signature>"
/// }
/// ```
///
/// The signed message is the deterministic ASCII string:
/// `bitrouter-siwg:{iss}:{nonce}:{iat}`
async fn resolve_siwg_identity(
    credential: &str,
    ctx: &JwtAuthContext,
) -> Result<Identity, warp::Rejection> {
    // Only attempt if the credential looks like a SIWG token.
    let encoded_payload = credential
        .strip_prefix(SIWG_CREDENTIAL_PREFIX)
        .ok_or_else(|| warp::reject::custom(Unauthorized("not a SIWG credential")))?;

    let Some(ref db) = ctx.db else {
        return Err(warp::reject::custom(Unauthorized(
            "SIWG authentication requires a database",
        )));
    };

    // 1. Decode the base64url payload.
    let payload_bytes = URL_SAFE_NO_PAD
        .decode(encoded_payload)
        .map_err(|_| warp::reject::custom(Unauthorized("invalid SIWG encoding")))?;

    #[derive(serde::Deserialize)]
    struct SiwgPayload {
        iss: String,
        nonce: String,
        iat: u64,
        sig: String,
    }

    let payload: SiwgPayload = serde_json::from_slice(&payload_bytes)
        .map_err(|_| warp::reject::custom(Unauthorized("invalid SIWG payload")))?;

    // 2. Parse CAIP-10 identity and determine algorithm.
    let caip10 = Caip10::parse(&payload.iss)
        .map_err(|_| warp::reject::custom(Unauthorized("invalid CAIP-10 in SIWG")))?;

    // 3. Reconstruct the signed message and verify signature.
    let signed_message = format!(
        "bitrouter-siwg:{}:{}:{}",
        payload.iss, payload.nonce, payload.iat
    );
    let sig_bytes = URL_SAFE_NO_PAD
        .decode(&payload.sig)
        .map_err(|_| warp::reject::custom(Unauthorized("invalid SIWG signature encoding")))?;

    let alg = caip10.chain.jwt_algorithm();
    let verify_result = match alg {
        JwtAlgorithm::SolEdDsa => {
            jwt_token::verify_sol_eddsa(signed_message.as_bytes(), &sig_bytes, &caip10.address)
        }
        JwtAlgorithm::Eip191K => {
            jwt_token::verify_eip191k(signed_message.as_bytes(), &sig_bytes, &caip10.address)
        }
    };
    verify_result
        .map_err(|_| warp::reject::custom(Unauthorized("SIWG signature verification failed")))?;

    // 4. Check freshness — iat must be within SIWG_MAX_AGE_SECS of now.
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|_| warp::reject::custom(Unauthorized("system clock error")))?
        .as_secs();

    let age = now.saturating_sub(payload.iat);
    if age > SIWG_MAX_AGE_SECS {
        return Err(warp::reject::custom(Unauthorized(
            "SIWG credential expired",
        )));
    }
    // Also reject credentials issued in the future (allows small clock skew).
    if payload.iat > now + 60 {
        return Err(warp::reject::custom(Unauthorized(
            "SIWG credential issued in the future",
        )));
    }

    // 5. Resolve account via CAIP-10 identity.
    let svc = AccountService::new(db);
    let account = svc
        .find_or_create_by_pubkey(&payload.iss)
        .await
        .map_err(|_| warp::reject::custom(Unauthorized("account lookup failed")))?;

    let Some(account) = account else {
        return Err(warp::reject::custom(Unauthorized(
            "public key has been rotated — use your current key",
        )));
    };

    // 6. SIWG always gets API scope (admin requires JWT with explicit scope).
    Ok(Identity {
        account_id: AccountId(account.id),
        scope: Scope::Api,
    })
}

// ── composite auth filters ────────────────────────────────────

/// Build an auth filter for OpenAI-protocol routes (`Authorization: Bearer`).
///
/// When auth is disabled (no DB), returns a passthrough identity.
pub fn openai_auth(
    ctx: Arc<JwtAuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    if ctx.is_open() {
        return open_identity().boxed();
    }
    let ctx = ctx.clone();
    bearer_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<JwtAuthContext>| async move {
            resolve_identity(&credential, &ctx).await
        })
        .boxed()
}

/// Build an auth filter for Anthropic-protocol routes (`x-api-key`).
///
/// When auth is disabled (no DB), returns a passthrough identity.
pub fn anthropic_auth(
    ctx: Arc<JwtAuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    if ctx.is_open() {
        return open_identity().boxed();
    }
    let ctx = ctx.clone();
    x_api_key_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<JwtAuthContext>| async move {
            resolve_identity(&credential, &ctx).await
        })
        .boxed()
}

/// Build an auth filter for management routes. Accepts both Bearer and x-api-key.
///
/// When auth is disabled (no DB), returns a passthrough identity.
pub fn management_auth(
    ctx: Arc<JwtAuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    if ctx.is_open() {
        return open_identity().boxed();
    }
    let ctx = ctx.clone();
    any_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<JwtAuthContext>| async move {
            resolve_identity(&credential, &ctx).await
        })
        .boxed()
}

/// Passthrough filter when auth is disabled — produces an anonymous admin identity.
fn open_identity() -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    warp::any().and_then(|| async {
        Ok::<_, warp::Rejection>(Identity {
            account_id: AccountId::new(),
            scope: Scope::Admin,
        })
    })
}

// ── rejection types ───────────────────────────────────────────

#[derive(Debug)]
pub struct Unauthorized(pub &'static str);

impl std::fmt::Display for Unauthorized {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "unauthorized: {}", self.0)
    }
}

impl warp::reject::Reject for Unauthorized {}
