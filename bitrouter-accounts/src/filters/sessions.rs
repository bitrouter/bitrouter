//! Session routes.
//!
//! These are scoped to the authenticated account — each caller can only
//! see and modify their own sessions.

use sea_orm::DatabaseConnection;
use serde::{Deserialize, Serialize};
use uuid::Uuid;
use warp::{Filter, Reply};

use crate::identity::Identity;
use crate::service::SessionService;

use super::accounts::{DbError, Forbidden};
use super::with_db;

/// Mount session routes under `/sessions`.
pub fn session_routes<A>(
    db: DatabaseConnection,
    auth: A,
) -> impl Filter<Extract = (impl Reply,), Error = warp::Rejection> + Clone
where
    A: Filter<Extract = (Identity,), Error = warp::Rejection> + Clone + Send + Sync + 'static,
{
    let list = warp::path!("sessions")
        .and(warp::get())
        .and(auth.clone())
        .and(with_db(db.clone()))
        .and_then(handle_list_sessions);

    let create = warp::path!("sessions")
        .and(warp::post())
        .and(auth.clone())
        .and(warp::body::json::<CreateSessionRequest>())
        .and(with_db(db.clone()))
        .and_then(handle_create_session);

    let get_messages = warp::path!("sessions" / Uuid / "messages")
        .and(warp::get())
        .and(auth.clone())
        .and(with_db(db.clone()))
        .and_then(handle_get_messages);

    let append_message = warp::path!("sessions" / Uuid / "messages")
        .and(warp::post())
        .and(auth.clone())
        .and(warp::body::json::<AppendMessageRequest>())
        .and(with_db(db.clone()))
        .and_then(handle_append_message);

    let delete = warp::path!("sessions" / Uuid)
        .and(warp::delete())
        .and(auth.clone())
        .and(with_db(db.clone()))
        .and_then(handle_delete_session);

    list.or(create)
        .or(get_messages)
        .or(append_message)
        .or(delete)
}

#[derive(Debug, Deserialize)]
struct CreateSessionRequest {
    title: Option<String>,
}

#[derive(Debug, Serialize)]
struct SessionResponse {
    id: String,
    title: Option<String>,
    created_at: String,
    updated_at: String,
}

#[derive(Debug, Deserialize)]
struct AppendMessageRequest {
    role: String,
    payload: serde_json::Value,
}

#[derive(Debug, Serialize)]
struct MessageResponse {
    id: String,
    position: i32,
    role: String,
    payload: serde_json::Value,
    created_at: String,
}

async fn handle_list_sessions(
    identity: Identity,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    let svc = SessionService::new(&db);
    let sessions = svc
        .list_sessions(identity.account_id)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    let body: Vec<SessionResponse> = sessions
        .into_iter()
        .map(|s| SessionResponse {
            id: s.id.to_string(),
            title: s.title,
            created_at: s.created_at.to_string(),
            updated_at: s.updated_at.to_string(),
        })
        .collect();

    Ok(warp::reply::json(&body))
}

async fn handle_create_session(
    identity: Identity,
    req: CreateSessionRequest,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    let svc = SessionService::new(&db);
    let session = svc
        .create_session(identity.account_id, req.title.as_deref())
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    let body = SessionResponse {
        id: session.id.to_string(),
        title: session.title,
        created_at: session.created_at.to_string(),
        updated_at: session.updated_at.to_string(),
    };

    Ok(warp::reply::with_status(
        warp::reply::json(&body),
        warp::http::StatusCode::CREATED,
    ))
}

async fn handle_get_messages(
    session_id: Uuid,
    identity: Identity,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    let svc = SessionService::new(&db);

    // Verify ownership.
    let session = svc
        .get_session(session_id)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?
        .ok_or_else(warp::reject::not_found)?;

    if session.account_id != identity.account_id.0 {
        return Err(warp::reject::custom(Forbidden));
    }

    let messages = svc
        .list_messages(session_id)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    let body: Vec<MessageResponse> = messages
        .into_iter()
        .map(|m| MessageResponse {
            id: m.id.to_string(),
            position: m.position,
            role: m.role,
            payload: serde_json::from_str(&m.payload).unwrap_or(serde_json::Value::Null),
            created_at: m.created_at.to_string(),
        })
        .collect();

    Ok(warp::reply::json(&body))
}

async fn handle_append_message(
    session_id: Uuid,
    identity: Identity,
    req: AppendMessageRequest,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    let svc = SessionService::new(&db);

    // Verify ownership.
    let session = svc
        .get_session(session_id)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?
        .ok_or_else(warp::reject::not_found)?;

    if session.account_id != identity.account_id.0 {
        return Err(warp::reject::custom(Forbidden));
    }

    let payload_str = serde_json::to_string(&req.payload).map_err(|_| {
        warp::reject::custom(DbError(sea_orm::DbErr::Custom("invalid payload".into())))
    })?;

    let msg = svc
        .append_message(session_id, &req.role, &payload_str)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    let body = MessageResponse {
        id: msg.id.to_string(),
        position: msg.position,
        role: msg.role,
        payload: serde_json::from_str(&msg.payload).unwrap_or(serde_json::Value::Null),
        created_at: msg.created_at.to_string(),
    };

    Ok(warp::reply::with_status(
        warp::reply::json(&body),
        warp::http::StatusCode::CREATED,
    ))
}

async fn handle_delete_session(
    session_id: Uuid,
    identity: Identity,
    db: DatabaseConnection,
) -> Result<impl Reply, warp::Rejection> {
    let svc = SessionService::new(&db);

    // Verify ownership.
    let session = svc
        .get_session(session_id)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?
        .ok_or_else(warp::reject::not_found)?;

    if session.account_id != identity.account_id.0 {
        return Err(warp::reject::custom(Forbidden));
    }

    svc.delete_session(session_id)
        .await
        .map_err(|e| warp::reject::custom(DbError(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({"deleted": true})),
        warp::http::StatusCode::OK,
    ))
}
