//! Warp filters for the admin A2A agent registry API.
//!
//! Provides HTTP endpoints for inspecting registered A2A agents:
//!
//! - `GET /admin/agents` — list all agents with connection status

use std::sync::Arc;

use bitrouter_a2a::admin::AdminAgentRegistry;
use warp::Filter;

/// Mount admin agent registry endpoints under `/admin/agents`.
///
/// Accepts `Option<Arc<T>>` — when `None` (no A2A configured), all endpoints
/// return 404. The caller is responsible for auth gating.
pub fn admin_agents_filter<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminAgentRegistry + 'static,
{
    list_agents(registry)
}

// ── GET /admin/agents ─────────────────────────────────────────────

fn list_agents<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminAgentRegistry + 'static,
{
    warp::path!("admin" / "agents")
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_list_agents)
}

async fn handle_list_agents<T: AdminAgentRegistry>(
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Err(warp::reject::not_found());
    };
    let agents = registry.list_agents().await;
    Ok(warp::reply::json(&serde_json::json!({ "agents": agents })))
}
