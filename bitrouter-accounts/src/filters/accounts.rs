//! Account management routes.
//!
//! All routes require [`Scope::Admin`] — the caller's auth filter decides
//! *how* admin access is verified. Admin scope is account-relative: callers
//! can only view/manage their own account.

use sea_orm::DatabaseConnection;
use serde::Serialize;
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
    warp::path!("accounts" / "me")
        .and(warp::get())
        .and(auth.clone())
        .and(with_db(db.clone()))
        .and_then(handle_get_own_account)
}

#[derive(Debug, Serialize)]
struct AccountResponse {
    id: String,
    name: String,
    master_pubkey: Option<String>,
    created_at: String,
}

async fn handle_get_own_account(
    identity: Identity,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    require_admin(&identity)?;
    let svc = AccountService::new(&db);
    let account = svc
        .get_account(identity.account_id)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    match account {
        Some(a) => {
            let body = AccountResponse {
                id: a.id.to_string(),
                name: a.name,
                master_pubkey: a.master_pubkey,
                created_at: a.created_at.to_string(),
            };
            Ok(warp::reply::json(&body))
        }
        None => Err(warp::reject::not_found()),
    }
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
