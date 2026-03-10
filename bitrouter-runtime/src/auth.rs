//! Authentication filters for the bitrouter gateway.
//!
//! Implements a LiteLLM-style key model:
//!
//! - A **master key** (configured in `bitrouter.yaml`) grants [`Scope::Admin`]
//!   access — it can call API endpoints and manage accounts/keys.
//! - **Virtual keys** (created via the `/key/generate` endpoint using the master
//!   key) grant [`Scope::Api`] access — they can call API endpoints only.
//!
//! Credentials are extracted from the protocol-appropriate header:
//!
//! | Protocol   | Header                          |
//! |------------|---------------------------------|
//! | OpenAI     | `Authorization: Bearer <key>`   |
//! | Anthropic  | `x-api-key: <key>`              |
//! | Management | `Authorization: Bearer <key>`   |
//!
//! When no `master_key` is configured, auth is disabled and all requests are
//! allowed through (open proxy mode).

use std::sync::Arc;

use sea_orm::DatabaseConnection;
use sha2::{Digest, Sha256};
use warp::Filter;

use bitrouter_accounts::identity::{AccountId, Identity, Scope};
use bitrouter_accounts::service::AccountService;

/// Shared auth state passed into filters.
#[derive(Clone)]
pub struct AuthContext {
    /// The configured master key (SHA-256 hash), if any.
    master_key_hash: Option<String>,
    /// Database connection for virtual key lookups.
    db: Option<DatabaseConnection>,
}

impl AuthContext {
    pub fn new(master_key: Option<&str>, db: Option<DatabaseConnection>) -> Self {
        Self {
            master_key_hash: master_key.map(hash_key),
            db,
        }
    }

    /// Returns `true` when no master key is configured (open proxy mode).
    pub fn is_open(&self) -> bool {
        self.master_key_hash.is_none()
    }
}

/// SHA-256 hash a key string, returning hex-encoded digest.
pub fn hash_key(key: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(key.as_bytes());
    hex::encode(hasher.finalize())
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
/// 1. If the credential matches the master key → Admin identity.
/// 2. Otherwise, look up the hash in the accounts DB → Api identity.
/// 3. If neither matches → reject.
async fn resolve_identity(
    credential: &str,
    ctx: &AuthContext,
) -> Result<Identity, warp::Rejection> {
    let credential_hash = hash_key(credential);

    // Check master key.
    if let Some(ref master_hash) = ctx.master_key_hash {
        if constant_time_eq(&credential_hash, master_hash) {
            return Ok(Identity {
                account_id: AccountId::new(),
                scope: Scope::Admin,
            });
        }
    }

    // Check virtual key in DB.
    if let Some(ref db) = ctx.db {
        let svc = AccountService::new(db);
        if let Ok(Some((account_id, _key))) = svc.resolve_api_key(&credential_hash).await {
            return Ok(Identity {
                account_id,
                scope: Scope::Api,
            });
        }
    }

    Err(warp::reject::custom(Unauthorized("invalid API key")))
}

/// Constant-time string comparison to prevent timing attacks.
fn constant_time_eq(a: &str, b: &str) -> bool {
    if a.len() != b.len() {
        return false;
    }
    a.bytes()
        .zip(b.bytes())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

// ── composite auth filters ────────────────────────────────────

/// Build an auth filter for OpenAI-protocol routes (`Authorization: Bearer`).
///
/// When auth is disabled (no master key), returns a passthrough identity.
pub fn openai_auth(
    ctx: Arc<AuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    if ctx.is_open() {
        return open_identity().boxed();
    }
    let ctx = ctx.clone();
    bearer_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<AuthContext>| async move {
            resolve_identity(&credential, &ctx).await
        })
        .boxed()
}

/// Build an auth filter for Anthropic-protocol routes (`x-api-key`).
///
/// When auth is disabled (no master key), returns a passthrough identity.
pub fn anthropic_auth(
    ctx: Arc<AuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    if ctx.is_open() {
        return open_identity().boxed();
    }
    let ctx = ctx.clone();
    x_api_key_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<AuthContext>| async move {
            resolve_identity(&credential, &ctx).await
        })
        .boxed()
}

/// Build an auth filter for management routes. Accepts both Bearer and x-api-key.
///
/// When auth is disabled (no master key), returns a passthrough identity.
pub fn management_auth(
    ctx: Arc<AuthContext>,
) -> impl Filter<Extract = (Identity,), Error = warp::Rejection> + Clone {
    if ctx.is_open() {
        return open_identity().boxed();
    }
    let ctx = ctx.clone();
    any_credential()
        .and(warp::any().map(move || ctx.clone()))
        .and_then(|credential: String, ctx: Arc<AuthContext>| async move {
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
