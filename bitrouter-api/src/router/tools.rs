//! Warp filter for the `GET /v1/tools` endpoint.
//!
//! Returns all tools available through the router, including
//! metadata such as name, description, and input schema.
//!
//! Supports optional query parameter filters:
//!
//! - `provider` — exact match on provider/server name
//! - `id` — substring match on tool ID (case-insensitive)

use std::sync::Arc;

use bitrouter_core::tools::registry::ToolRegistry;
use serde::Serialize;
use warp::Filter;

/// Query parameters for filtering the tool list.
#[derive(Debug, Default)]
struct ToolQuery {
    /// Filter by provider/server name (exact match).
    provider: Option<String>,
    /// Filter by tool ID (substring match, case-insensitive).
    id: Option<String>,
}

/// Creates a warp filter for `GET /v1/tools`.
///
/// Accepts `Option<Arc<T>>` — when `None` (no tool source configured), the
/// endpoint returns 404.
pub fn tools_filter<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: ToolRegistry + 'static,
{
    warp::path!("v1" / "tools")
        .and(warp::get())
        .and(optional_raw_query())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_list_tools)
}

/// Extracts the raw query string as `Option<String>`. Returns `None` when
/// the request has no query component instead of rejecting.
fn optional_raw_query()
-> impl Filter<Extract = (Option<String>,), Error = std::convert::Infallible> + Clone {
    warp::query::raw()
        .map(Some)
        .or(warp::any().map(|| None))
        .unify()
}

#[derive(Serialize)]
struct ToolResponse {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    input_schema: Option<serde_json::Value>,
}

fn parse_query(raw: &str) -> ToolQuery {
    let mut query = ToolQuery::default();
    for pair in raw.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "provider" => query.provider = Some(value.to_owned()),
                "id" => query.id = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    query
}

async fn handle_list_tools<T: ToolRegistry>(
    raw_query: Option<String>,
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Err(warp::reject::not_found());
    };
    let query = raw_query.as_deref().map(parse_query).unwrap_or_default();
    let entries = registry.list_tools().await;
    let id_lower = query.id.as_deref().map(str::to_lowercase);

    let tools: Vec<ToolResponse> = entries
        .into_iter()
        .filter(|e| {
            if query.provider.as_deref().is_some_and(|p| e.provider != p) {
                return false;
            }
            if id_lower
                .as_deref()
                .is_some_and(|s| !e.id.to_lowercase().contains(s))
            {
                return false;
            }
            true
        })
        .map(|e| {
            let input_schema = e
                .definition
                .input_schema
                .and_then(|s| serde_json::to_value(s).ok());
            ToolResponse {
                id: e.id,
                name: Some(e.definition.name),
                provider: e.provider,
                description: e.definition.description,
                input_schema,
            }
        })
        .collect();
    Ok(warp::reply::json(&serde_json::json!({ "tools": tools })))
}
