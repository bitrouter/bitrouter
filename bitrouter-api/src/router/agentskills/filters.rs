//! Warp filters for the `/v1/skills` CRUD endpoints.
//!
//! Follows the Anthropic Skills API shape:
//! - `POST   /v1/skills`          — register a skill
//! - `GET    /v1/skills`          — list skills
//! - `GET    /v1/skills/:name`    — retrieve a skill
//! - `DELETE /v1/skills/:name`    — delete a skill

use std::sync::Arc;

use bitrouter_core::routers::registry::{SkillEntry, SkillService};
use serde::{Deserialize, Serialize};
use warp::Filter;

// ── Request / response types ────────────────────────────────────────

#[derive(Deserialize)]
struct CreateSkillRequest {
    name: String,
    description: String,
    #[serde(default)]
    source: Option<String>,
    #[serde(default)]
    required_apis: Vec<String>,
}

#[derive(Serialize)]
struct SkillResponse {
    id: String,
    #[serde(rename = "type")]
    type_field: &'static str,
    name: String,
    description: String,
    source: String,
    required_apis: Vec<String>,
    created_at: String,
    updated_at: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    bound_tool: Option<String>,
}

impl From<SkillEntry> for SkillResponse {
    fn from(e: SkillEntry) -> Self {
        Self {
            id: e.id,
            type_field: "skill",
            name: e.name,
            description: e.description,
            source: e.source,
            required_apis: e.required_apis,
            created_at: e.created_at,
            updated_at: e.updated_at,
            bound_tool: e.bound_tool,
        }
    }
}

#[derive(Serialize)]
struct ListSkillsResponse {
    data: Vec<SkillResponse>,
}

#[derive(Serialize)]
struct DeleteSkillResponse {
    id: String,
    #[serde(rename = "type")]
    type_field: &'static str,
}

#[derive(Serialize)]
struct ErrorResponse {
    error: ErrorBody,
}

#[derive(Serialize)]
struct ErrorBody {
    message: String,
    #[serde(rename = "type")]
    error_type: &'static str,
}

// ── Filters ─────────────────────────────────────────────────────────

/// Combined filter for all `/v1/skills` endpoints.
pub fn skills_filter<S>(
    service: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: SkillService + 'static,
{
    create_skill(service.clone())
        .or(list_skills(service.clone()))
        .or(get_skill(service.clone()))
        .or(delete_skill(service))
}

fn create_skill<S>(
    service: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: SkillService + 'static,
{
    warp::path!("v1" / "skills")
        .and(warp::post())
        .and(warp::body::json::<CreateSkillRequest>())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_create)
}

fn list_skills<S>(
    service: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: SkillService + 'static,
{
    warp::path!("v1" / "skills")
        .and(warp::get())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_list)
}

fn get_skill<S>(
    service: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: SkillService + 'static,
{
    warp::path!("v1" / "skills" / String)
        .and(warp::get())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_get)
}

fn delete_skill<S>(
    service: Arc<S>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    S: SkillService + 'static,
{
    warp::path!("v1" / "skills" / String)
        .and(warp::delete())
        .and(warp::any().map(move || service.clone()))
        .and_then(handle_delete)
}

// ── Handlers ────────────────────────────────────────────────────────

async fn handle_create<S: SkillService>(
    body: CreateSkillRequest,
    service: Arc<S>,
) -> Result<impl warp::Reply, warp::Rejection> {
    match service
        .create(body.name, body.description, body.source, body.required_apis)
        .await
    {
        Ok(entry) => Ok(warp::reply::with_status(
            warp::reply::json(&SkillResponse::from(entry)),
            warp::http::StatusCode::CREATED,
        )),
        Err(msg) => Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse {
                error: ErrorBody {
                    message: msg,
                    error_type: "invalid_request_error",
                },
            }),
            warp::http::StatusCode::BAD_REQUEST,
        )),
    }
}

async fn handle_list<S: SkillService>(
    service: Arc<S>,
) -> Result<impl warp::Reply, warp::Rejection> {
    match service.list().await {
        Ok(entries) => {
            let data = entries.into_iter().map(SkillResponse::from).collect();
            Ok(warp::reply::with_status(
                warp::reply::json(&ListSkillsResponse { data }),
                warp::http::StatusCode::OK,
            ))
        }
        Err(msg) => Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse {
                error: ErrorBody {
                    message: msg,
                    error_type: "api_error",
                },
            }),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

async fn handle_get<S: SkillService>(
    name: String,
    service: Arc<S>,
) -> Result<impl warp::Reply, warp::Rejection> {
    match service.get(&name).await {
        Ok(Some(entry)) => Ok(warp::reply::with_status(
            warp::reply::json(&SkillResponse::from(entry)),
            warp::http::StatusCode::OK,
        )),
        Ok(None) => Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse {
                error: ErrorBody {
                    message: format!("skill '{name}' not found"),
                    error_type: "not_found_error",
                },
            }),
            warp::http::StatusCode::NOT_FOUND,
        )),
        Err(msg) => Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse {
                error: ErrorBody {
                    message: msg,
                    error_type: "api_error",
                },
            }),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}

async fn handle_delete<S: SkillService>(
    name: String,
    service: Arc<S>,
) -> Result<impl warp::Reply, warp::Rejection> {
    match service.delete(&name).await {
        Ok(true) => Ok(warp::reply::with_status(
            warp::reply::json(&DeleteSkillResponse {
                id: name,
                type_field: "skill_deleted",
            }),
            warp::http::StatusCode::OK,
        )),
        Ok(false) => Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse {
                error: ErrorBody {
                    message: format!("skill '{name}' not found"),
                    error_type: "not_found_error",
                },
            }),
            warp::http::StatusCode::NOT_FOUND,
        )),
        Err(msg) => Ok(warp::reply::with_status(
            warp::reply::json(&ErrorResponse {
                error: ErrorBody {
                    message: msg,
                    error_type: "api_error",
                },
            }),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        )),
    }
}
