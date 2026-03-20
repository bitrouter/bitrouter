//! Warp filters for the admin tool management API.
//!
//! Provides HTTP endpoints for managing MCP tools at runtime:
//!
//! - `GET /admin/tools` — list all aggregated tools
//! - `PUT /admin/tools/:server/filter` — update allow/deny for a server
//! - `GET /admin/tools/upstreams` — list upstream servers with status
//! - `GET /admin/tools/groups` — list configured access groups
//! - `PUT /admin/tools/:server/params` — update parameter restrictions

use std::sync::Arc;

use bitrouter_mcp::admin::AdminToolRegistry;
use bitrouter_mcp::config::ToolFilter;
use bitrouter_mcp::param_filter::ParamRestrictions;
use warp::Filter;

/// Mount all admin tool management endpoints under `/admin/tools`.
///
/// Accepts `Option<Arc<T>>` — when `None` (no MCP configured), all endpoints
/// return 404. The caller is responsible for auth gating.
pub fn admin_tools_filter<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    list_tools(registry.clone())
        .or(list_upstreams(registry.clone()))
        .or(list_groups(registry.clone()))
        .or(update_filter(registry.clone()))
        .or(update_params(registry))
}

// ── GET /admin/tools ────────────────────────────────────────────────

fn list_tools<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "tools")
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_list_tools)
}

async fn handle_list_tools<T: AdminToolRegistry>(
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Err(warp::reject::not_found());
    };
    let tools = registry.list_tools().await;
    Ok(warp::reply::json(&serde_json::json!({ "tools": tools })))
}

// ── GET /admin/tools/upstreams ──────────────────────────────────────

fn list_upstreams<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "tools" / "upstreams")
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_list_upstreams)
}

async fn handle_list_upstreams<T: AdminToolRegistry>(
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Err(warp::reject::not_found());
    };
    let upstreams = registry.list_upstreams().await;
    Ok(warp::reply::json(
        &serde_json::json!({ "upstreams": upstreams }),
    ))
}

// ── GET /admin/tools/groups ─────────────────────────────────────────

fn list_groups<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "tools" / "groups")
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_list_groups)
}

async fn handle_list_groups<T: AdminToolRegistry>(
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Err(warp::reject::not_found());
    };
    let groups = registry.list_groups().await;
    Ok(warp::reply::json(&serde_json::json!({ "groups": groups })))
}

// ── PUT /admin/tools/:server/filter ─────────────────────────────────

fn update_filter<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "tools" / String / "filter")
        .and(warp::put())
        .and(warp::body::json::<FilterUpdateBody>())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_update_filter)
}

#[derive(serde::Deserialize)]
struct FilterUpdateBody {
    #[serde(default)]
    allow: Option<Vec<String>>,
    #[serde(default)]
    deny: Option<Vec<String>>,
}

async fn handle_update_filter<T: AdminToolRegistry>(
    server: String,
    body: FilterUpdateBody,
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": { "message": "MCP gateway not configured" }
            })),
            warp::http::StatusCode::NOT_FOUND,
        ));
    };

    let filter = if body.allow.is_some() || body.deny.is_some() {
        Some(ToolFilter {
            allow: body.allow,
            deny: body.deny,
        })
    } else {
        None
    };

    match registry.update_filter(&server, filter).await {
        Ok(()) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "status": "ok",
                "server": server,
            })),
            warp::http::StatusCode::OK,
        )),
        Err(e) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": { "message": e.to_string() }
            })),
            warp::http::StatusCode::NOT_FOUND,
        )),
    }
}

// ── PUT /admin/tools/:server/params ─────────────────────────────────

fn update_params<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "tools" / String / "params")
        .and(warp::put())
        .and(warp::body::json::<ParamRestrictions>())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_update_params)
}

async fn handle_update_params<T: AdminToolRegistry>(
    server: String,
    restrictions: ParamRestrictions,
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": { "message": "MCP gateway not configured" }
            })),
            warp::http::StatusCode::NOT_FOUND,
        ));
    };

    match registry
        .update_param_restrictions(&server, restrictions)
        .await
    {
        Ok(()) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "status": "ok",
                "server": server,
            })),
            warp::http::StatusCode::OK,
        )),
        Err(e) => Ok(warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": { "message": e.to_string() }
            })),
            warp::http::StatusCode::NOT_FOUND,
        )),
    }
}
