//! Warp filters for MCP server management.
//!
//! Provides HTTP endpoints for managing upstream MCP servers at runtime:
//!
//! - `GET /admin/mcp/tools` — list all aggregated tools
//! - `GET /admin/mcp/servers` — list upstream servers with status
//! - `PUT /admin/mcp/servers/:name/filter` — update allow/deny for a server
//! - `PUT /admin/mcp/servers/:name/params` — update parameter restrictions
//!
//! Generic over [`AdminToolRegistry`] — no protocol-crate dependency.

use std::sync::Arc;

use bitrouter_core::routers::admin::{AdminToolRegistry, ParamRestrictions, ToolFilter};
use warp::Filter;

/// Mount all MCP admin endpoints under `/admin/mcp`.
///
/// Accepts `Option<Arc<T>>` — when `None` (no MCP configured), all
/// endpoints return 404. The caller is responsible for auth gating.
pub fn mcp_admin_filter<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    list_tools(registry.clone())
        .or(list_servers(registry.clone()))
        .or(update_filter(registry.clone()))
        .or(update_params(registry))
}

// ── GET /admin/mcp/tools ───────────────────────────────────────────

fn list_tools<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "mcp" / "tools")
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

    // Map core ToolEntry to a serializable response.
    let items: Vec<serde_json::Value> = tools
        .into_iter()
        .map(|t| {
            let input_schema = t
                .definition
                .input_schema
                .and_then(|s| serde_json::to_value(s).ok());
            serde_json::json!({
                "id": t.id,
                "name": t.definition.name,
                "provider": t.provider,
                "description": t.definition.description,
                "input_schema": input_schema,
            })
        })
        .collect();
    Ok(warp::reply::json(&serde_json::json!({ "tools": items })))
}

// ── GET /admin/mcp/servers ─────────────────────────────────────────

fn list_servers<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "mcp" / "servers")
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_list_servers)
}

async fn handle_list_servers<T: AdminToolRegistry>(
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Err(warp::reject::not_found());
    };
    let servers = registry.list_upstreams().await;
    Ok(warp::reply::json(
        &serde_json::json!({ "servers": servers }),
    ))
}

// ── PUT /admin/mcp/servers/:name/filter ────────────────────────────

fn update_filter<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "mcp" / "servers" / String / "filter")
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
                "error": { "message": "MCP registry not configured" }
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

// ── PUT /admin/mcp/servers/:name/params ────────────────────────────

fn update_params<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminToolRegistry + 'static,
{
    warp::path!("admin" / "mcp" / "servers" / String / "params")
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
                "error": { "message": "MCP registry not configured" }
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
