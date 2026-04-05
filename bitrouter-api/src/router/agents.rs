//! Warp filter for the `GET /v1/agents` endpoint.
//!
//! Returns all agents available through the router, including metadata
//! such as protocol, status, and capability flags.

use std::sync::Arc;

use bitrouter_core::routers::registry::AgentRegistry;
use serde::Serialize;
use warp::Filter;

/// Creates a warp filter for `GET /v1/agents`.
///
/// Accepts `Option<Arc<T>>` — when `None` (no agent source configured), the
/// endpoint returns 404.
pub fn agents_filter<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AgentRegistry + 'static,
{
    warp::path!("v1" / "agents")
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_list_agents)
}

#[derive(Serialize)]
struct AgentResponse {
    name: String,
    protocol: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    status: String,
    capabilities: AgentCapabilitiesResponse,
}

#[derive(Serialize)]
struct AgentCapabilitiesResponse {
    supports_permissions: bool,
    supports_thinking: bool,
    supports_tool_calls: bool,
}

async fn handle_list_agents<T: AgentRegistry>(
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Err(warp::reject::not_found());
    };
    let entries = registry.list_agents().await;

    let agents: Vec<AgentResponse> = entries
        .into_iter()
        .map(|e| {
            let status = match e.status {
                bitrouter_core::routers::registry::AgentEntryStatus::Idle => "idle",
                bitrouter_core::routers::registry::AgentEntryStatus::Connected => "connected",
                bitrouter_core::routers::registry::AgentEntryStatus::Unavailable => "unavailable",
            };
            AgentResponse {
                name: e.name,
                protocol: e.protocol,
                description: e.description,
                status: status.to_owned(),
                capabilities: AgentCapabilitiesResponse {
                    supports_permissions: e.capabilities.supports_permissions,
                    supports_thinking: e.capabilities.supports_thinking,
                    supports_tool_calls: e.capabilities.supports_tool_calls,
                },
            }
        })
        .collect();
    Ok(warp::reply::json(&serde_json::json!({ "agents": agents })))
}
