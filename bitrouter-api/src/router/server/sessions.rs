//! Warp filters for session management endpoints.

use std::sync::Arc;

use bitrouter_core::server::{
    ids::{AccountId, SessionId},
    pagination::{CursorPage, PageRequest},
    sessions::{
        CreateSessionRequest, SessionDetail, SessionMutation, SessionQueryService, SessionSummary,
        SessionWriteService,
    },
};
use serde::{Deserialize, Serialize};
use warp::Filter;

use crate::error::ServerRejection;

// ---------------------------------------------------------------------------
// Request / response DTOs
// ---------------------------------------------------------------------------

#[derive(Deserialize)]
struct CreateSessionBody {
    account_id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: serde_json::Value,
}

#[derive(Deserialize)]
struct UpdateSessionBody {
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    content: Option<serde_json::Value>,
}

#[derive(Deserialize)]
struct SessionListQuery {
    account_id: String,
    #[serde(default)]
    cursor: Option<String>,
    #[serde(default = "default_limit")]
    limit: u32,
}

fn default_limit() -> u32 {
    20
}

#[derive(Serialize)]
struct SessionSummaryResponse {
    id: String,
    account_id: String,
    title: Option<String>,
    created_at: i64,
    updated_at: i64,
}

#[derive(Serialize)]
struct SessionDetailResponse {
    id: String,
    account_id: String,
    title: Option<String>,
    content: serde_json::Value,
    created_at: i64,
    updated_at: i64,
}

#[derive(Serialize)]
struct PageResponse<T: Serialize> {
    items: Vec<T>,
    next_cursor: Option<String>,
    has_more: bool,
}

// ---------------------------------------------------------------------------
// Conversion helpers
// ---------------------------------------------------------------------------

fn summary_to_response(s: SessionSummary) -> SessionSummaryResponse {
    SessionSummaryResponse {
        id: s.id.to_string(),
        account_id: s.account_id.to_string(),
        title: s.title,
        created_at: s.created_at.as_secs(),
        updated_at: s.updated_at.as_secs(),
    }
}

fn detail_to_response(d: SessionDetail) -> SessionDetailResponse {
    SessionDetailResponse {
        id: d.summary.id.to_string(),
        account_id: d.summary.account_id.to_string(),
        title: d.summary.title,
        content: d.content,
        created_at: d.summary.created_at.as_secs(),
        updated_at: d.summary.updated_at.as_secs(),
    }
}

// ---------------------------------------------------------------------------
// Filters
// ---------------------------------------------------------------------------

/// POST /v1/sessions — create a session.
pub fn create_session_filter<W>(
    service: Arc<W>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    W: SessionWriteService + Send + Sync + 'static,
{
    warp::path!("v1" / "sessions")
        .and(warp::post())
        .and(warp::body::json::<CreateSessionBody>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_create_session)
}

/// GET /v1/sessions/:id — get a single session.
pub fn get_session_filter<Q>(
    service: Arc<Q>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    Q: SessionQueryService + Send + Sync + 'static,
{
    warp::path!("v1" / "sessions" / String)
        .and(warp::get())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_get_session)
}

/// GET /v1/sessions?account_id=...&cursor=...&limit=... — list sessions.
pub fn list_sessions_filter<Q>(
    service: Arc<Q>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    Q: SessionQueryService + Send + Sync + 'static,
{
    warp::path!("v1" / "sessions")
        .and(warp::get())
        .and(warp::query::<SessionListQuery>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_list_sessions)
}

/// PATCH /v1/sessions/:id — update a session.
pub fn update_session_filter<W>(
    service: Arc<W>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    W: SessionWriteService + Send + Sync + 'static,
{
    warp::path!("v1" / "sessions" / String)
        .and(warp::patch())
        .and(warp::body::json::<UpdateSessionBody>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_update_session)
}

/// DELETE /v1/sessions/:id — delete a session.
pub fn delete_session_filter<W>(
    service: Arc<W>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    W: SessionWriteService + Send + Sync + 'static,
{
    warp::path!("v1" / "sessions" / String)
        .and(warp::delete())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_delete_session)
}

// ---------------------------------------------------------------------------
// Handlers
// ---------------------------------------------------------------------------

async fn handle_create_session<W>(
    body: CreateSessionBody,
    service: Arc<W>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    W: SessionWriteService + Send + Sync + 'static,
{
    let detail = service
        .create_session(CreateSessionRequest {
            account_id: AccountId::new(body.account_id),
            title: body.title,
            content: body.content,
        })
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&detail_to_response(detail)),
        warp::http::StatusCode::CREATED,
    ))
}

async fn handle_get_session<Q>(
    id: String,
    service: Arc<Q>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    Q: SessionQueryService + Send + Sync + 'static,
{
    let detail = service
        .get_session(&SessionId::new(id))
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::json(&detail_to_response(detail)))
}

async fn handle_list_sessions<Q>(
    query: SessionListQuery,
    service: Arc<Q>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    Q: SessionQueryService + Send + Sync + 'static,
{
    let page = service
        .list_sessions(
            &AccountId::new(query.account_id),
            PageRequest {
                cursor: query.cursor,
                limit: query.limit,
            },
        )
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::json(&cursor_page_response(page)))
}

async fn handle_update_session<W>(
    id: String,
    body: UpdateSessionBody,
    service: Arc<W>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    W: SessionWriteService + Send + Sync + 'static,
{
    let detail = service
        .update_session(
            &SessionId::new(id),
            SessionMutation {
                title: body.title,
                content: body.content,
            },
        )
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::json(&detail_to_response(detail)))
}

async fn handle_delete_session<W>(
    id: String,
    service: Arc<W>,
) -> Result<impl warp::Reply, warp::Rejection>
where
    W: SessionWriteService + Send + Sync + 'static,
{
    service
        .delete_session(&SessionId::new(id))
        .await
        .map_err(|e| warp::reject::custom(ServerRejection(e)))?;

    Ok(warp::reply::with_status(
        warp::reply::json(&serde_json::json!({})),
        warp::http::StatusCode::NO_CONTENT,
    ))
}

// ---------------------------------------------------------------------------
// Shared helpers
// ---------------------------------------------------------------------------

fn cursor_page_response(page: CursorPage<SessionSummary>) -> PageResponse<SessionSummaryResponse> {
    PageResponse {
        items: page.items.into_iter().map(summary_to_response).collect(),
        next_cursor: page.next_cursor,
        has_more: page.has_more,
    }
}
