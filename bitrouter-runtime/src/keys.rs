//! HTTP endpoints for API key lifecycle.
//!
//! Mounted at `/key/*`, these allow callers with **admin** access (master key)
//! to create and revoke virtual keys.

use std::sync::Arc;

use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use warp::Filter;

use bitrouter_accounts::identity::{Identity, Scope};
use bitrouter_accounts::service::AccountService;

use crate::auth::{self, AuthContext, Unauthorized, hash_key};

// ── request / response DTOs ───────────────────────────────────

#[derive(Debug, Deserialize)]
pub struct GenerateKeyRequest {
    /// Human-readable name for the key.
    #[serde(default = "default_key_name")]
    pub name: String,
    /// Account to associate with. If omitted, a new account is created.
    pub account_id: Option<Uuid>,
}

fn default_key_name() -> String {
    "default".into()
}

#[derive(Debug, Serialize)]
pub struct GenerateKeyResponse {
    /// The plaintext key — **only returned once**.
    pub key: String,
    /// Display prefix (e.g. `sk-br-12...`).
    pub prefix: String,
    /// The account that owns this key.
    pub account_id: Uuid,
}

#[derive(Debug, Deserialize)]
pub struct RevokeKeyRequest {
    pub key_id: Uuid,
}

#[derive(Debug, Serialize)]
pub struct RevokeKeyResponse {
    pub revoked: bool,
}

// ── route builder ─────────────────────────────────────────────

/// Build the `/key/generate` and `/key/revoke` routes.
///
/// Both require admin scope (master key). If no DB is configured, the routes
/// will still be mounted but will reject with a 503.
pub fn key_routes(
    auth_ctx: Arc<AuthContext>,
    db: Option<Arc<DatabaseConnection>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone {
    let generate = warp::path!("key" / "generate")
        .and(warp::post())
        .and(auth::management_auth(auth_ctx.clone()))
        .and(warp::body::json::<GenerateKeyRequest>())
        .and(require_db(db.clone()))
        .and_then(handle_generate);

    let revoke = warp::path!("key" / "revoke")
        .and(warp::post())
        .and(auth::management_auth(auth_ctx))
        .and(warp::body::json::<RevokeKeyRequest>())
        .and(require_db(db))
        .and_then(handle_revoke);

    generate.or(revoke)
}

fn require_db(
    db: Option<Arc<DatabaseConnection>>,
) -> impl Filter<Extract = (Arc<DatabaseConnection>,), Error = warp::Rejection> + Clone {
    warp::any().and_then(move || {
        let db = db.clone();
        async move {
            db.ok_or_else(|| warp::reject::custom(KeyError("database not configured".into())))
        }
    })
}

// ── handlers ──────────────────────────────────────────────────

async fn handle_generate(
    identity: Identity,
    req: GenerateKeyRequest,
    db: Arc<DatabaseConnection>,
) -> Result<impl warp::Reply, warp::Rejection> {
    require_admin(&identity)?;

    let svc = AccountService::new(&db);

    // Resolve or create account.
    let account_id = match req.account_id {
        Some(id) => bitrouter_accounts::identity::AccountId(id),
        None => {
            let acct = svc
                .create_account(&req.name)
                .await
                .map_err(|e| warp::reject::custom(KeyError(e.to_string())))?;
            bitrouter_accounts::identity::AccountId(acct.id)
        }
    };

    // Generate a random key with a recognizable prefix.
    let plaintext = format!("sk-br-{}", Uuid::new_v4().as_simple());
    let hashed = hash_key(&plaintext);

    let model = svc
        .create_api_key(account_id, &req.name, &plaintext, &hashed)
        .await
        .map_err(|e| warp::reject::custom(KeyError(e.to_string())))?;

    Ok(warp::reply::json(&GenerateKeyResponse {
        key: plaintext,
        prefix: model.prefix,
        account_id: account_id.0,
    }))
}

async fn handle_revoke(
    identity: Identity,
    req: RevokeKeyRequest,
    db: Arc<DatabaseConnection>,
) -> Result<impl warp::Reply, warp::Rejection> {
    require_admin(&identity)?;

    let svc = AccountService::new(&db);
    svc.revoke_api_key(req.key_id)
        .await
        .map_err(|e| warp::reject::custom(KeyError(e.to_string())))?;

    Ok(warp::reply::json(&RevokeKeyResponse { revoked: true }))
}

fn require_admin(identity: &Identity) -> Result<(), warp::Rejection> {
    if identity.scope >= Scope::Admin {
        Ok(())
    } else {
        Err(warp::reject::custom(Unauthorized("admin access required")))
    }
}

// ── error type ────────────────────────────────────────────────

#[derive(Debug)]
struct KeyError(String);

impl std::fmt::Display for KeyError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "key operation failed: {}", self.0)
    }
}

impl warp::reject::Reject for KeyError {}
