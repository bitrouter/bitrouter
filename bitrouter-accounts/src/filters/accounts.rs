//! Account management routes.
//!
//! All routes require [`Scope::Admin`] — the caller's auth filter decides
//! *how* admin access is verified.

use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use warp::{Filter, Reply};

use crate::identity::{Identity, Scope};
use crate::service::AccountService;

use super::with_db;

/// Mount account management routes under `/accounts`.
pub fn account_routes<A>(
    db: DatabaseConnection,
    auth: A,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (Identity,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    let list = warp::path!("accounts")
        .and(warp::get())
        .and(auth.clone())
        .and(with_db(db.clone()))
        .and_then(handle_list_accounts);

    let create = warp::path!("accounts")
        .and(warp::post())
        .and(auth.clone())
        .and(warp::body::json::<CreateAccountRequest>())
        .and(with_db(db.clone()))
        .and_then(handle_create_account);

    let list_keys = warp::path!("accounts" / "keys")
        .and(warp::get())
        .and(auth.clone())
        .and(with_db(db.clone()))
        .and_then(handle_list_api_keys);

    list.or(create).or(list_keys)
}

#[derive(Debug, Deserialize)]
struct CreateAccountRequest {
    name: String,
}

#[derive(Debug, Serialize)]
struct AccountResponse {
    id: String,
    name: String,
    created_at: String,
}

async fn handle_list_accounts(
    identity: Identity,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    require_admin(&identity)?;
    let svc = AccountService::new(&db);
    let accounts = svc
        .list_accounts()
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    let body: Vec<AccountResponse> = accounts
        .into_iter()
        .map(|a| AccountResponse {
            id: a.id.to_string(),
            name: a.name,
            created_at: a.created_at.to_string(),
        })
        .collect();

    Ok(warp::reply::json(&body))
}

async fn handle_create_account(
    identity: Identity,
    req: CreateAccountRequest,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    require_admin(&identity)?;
    let svc = AccountService::new(&db);
    let account = svc
        .create_account(&req.name)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    let body = AccountResponse {
        id: account.id.to_string(),
        name: account.name,
        created_at: account.created_at.to_string(),
    };

    Ok(warp::reply::with_status(
        warp::reply::json(&body),
        warp::http::StatusCode::CREATED,
    ))
}

async fn handle_list_api_keys(
    identity: Identity,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    require_admin(&identity)?;
    let svc = AccountService::new(&db);
    let keys = svc
        .list_api_keys(identity.account_id)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    let body: Vec<serde_json::Value> = keys
        .into_iter()
        .map(|k| {
            serde_json::json!({
                "id": k.id.to_string(),
                "name": k.name,
                "prefix": k.prefix,
                "created_at": k.created_at.to_string(),
                "expires_at": k.expires_at.map(|t| t.to_string()),
            })
        })
        .collect();

    Ok(warp::reply::json(&body))
}

fn require_admin(identity: &Identity) -> Result<(), warp::Rejection> {
    if identity.scope < Scope::Admin {
        return Err(warp::reject::custom(Forbidden));
    }
    Ok(())
}

// ── rejection types ───────────────────────────────────────────

#[derive(Debug)]
pub(crate) struct DbError(pub sea_orm::DbErr);
impl std::fmt::Display for DbError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "database error: {}", self.0)
    }
}
impl warp::reject::Reject for DbError {}

#[derive(Debug)]
pub(crate) struct Forbidden;
impl warp::reject::Reject for Forbidden {}
