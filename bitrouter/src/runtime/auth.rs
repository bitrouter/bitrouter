//! JWT authentication filters for the bitrouter gateway.
//!
//! Implements operator-signed JWT authentication:
//!
//! - **JWT path**: The operator's OWS wallet signs all JWTs. The `iss`
//!   claim carries the operator's CAIP-10 identity. The server verifies
//!   the signature and checks that `iss` matches the configured operator
//!   wallet — this is the single trust root.
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

use sea_orm::DatabaseConnection;
use warp::Filter;

use bitrouter_accounts::identity::{AccountId, Identity, Scope};
use bitrouter_accounts::service::AccountService;
use bitrouter_core::auth::chain::Caip10;
use bitrouter_core::auth::claims::TokenScope;
use bitrouter_core::auth::token as jwt_token;

/// Shared auth state passed into filters.
#[derive(Clone)]
pub struct JwtAuthContext {
    /// Database connection for account lookups (auto-creation).
    db: Option<DatabaseConnection>,
    /// The operator's CAIP-10 identity resolved from wallet config at startup.
    /// When set, JWTs must have `iss` matching this identity.
    operator_caip10: Option<String>,
}

impl JwtAuthContext {
    pub fn new(db: Option<DatabaseConnection>, operator_caip10: Option<String>) -> Self {
        Self {
            db,
            operator_caip10,
        }
    }

    /// Returns `true` when no database is configured (open proxy mode).
    pub fn is_open(&self) -> bool {
        self.db.is_none()
    }
}

// ── credential extraction ─────────────────────────────────────

/// Extract a bearer token from `Authorization: Bearer <token>`.
///
/// Handles comma-separated schemes so that
/// `"Bearer <jwt>, Payment <cred>"` correctly returns `"<jwt>"`.
fn extract_bearer(header: &str) -> Option<&str> {
    for segment in header.split(',') {
        let trimmed = segment.trim();
        if let Some(token) = trimmed
            .strip_prefix("Bearer ")
            .or_else(|| trimmed.strip_prefix("bearer "))
        {
            return Some(token.trim());
        }
    }
    None
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
/// Verifies the JWT signature, checks expiration, and resolves the account.
///
/// Security-critical ordering:
/// 1. Verify signature cryptographically.
/// 2. Check expiration.
/// 3. Look up / auto-create account in DB using CAIP-10 identity.
/// 4. Build Identity with account-relative scope.
async fn resolve_identity(
    credential: &str,
    ctx: &JwtAuthContext,
) -> Result<Identity, warp::Rejection> {
    resolve_jwt_identity(credential, ctx).await
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

    // 3. Verify iss matches the configured operator wallet (single trust root).
    if let Some(ref expected) = ctx.operator_caip10
        && claims.iss != *expected
    {
        return Err(warp::reject::custom(Unauthorized(
            "JWT issuer does not match configured operator wallet",
        )));
    }

    // 4. Derive chain from iss (CAIP-10 → CAIP-2).
    let chain = Caip10::parse(&claims.iss).ok().map(|c| c.chain.caip2());

    // 5. Resolve account from DB using CAIP-10 iss.
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

    // 6. Build identity with resolved scope and permissions.
    let scope = match claims.scope() {
        TokenScope::Admin => Scope::Admin,
        TokenScope::Api => Scope::Api,
    };

    Ok(Identity {
        account_id: AccountId(account.id),
        scope,
        chain,
        models: claims.mdl,
        budget: claims.bgt,
        budget_scope: claims.bsc,
        key: claims.key,
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
///
/// Uses a deterministic all-zero UUID so open-mode spend logs are consistently
/// attributable rather than scattered across random IDs.
fn open_identity() -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    warp::any().and_then(|| async {
        Ok::<_, warp::Rejection>(Identity {
            account_id: AccountId(uuid::Uuid::nil()),
            scope: Scope::Admin,
            chain: None,
            models: None,
            budget: None,
            budget_scope: None,
            key: None,
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
