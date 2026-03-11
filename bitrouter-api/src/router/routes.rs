//! Warp filter for the `GET /v1/routes` endpoint.
//!
//! Returns the list of configured model routes, including the virtual model
//! name, provider, and API protocol for each route.

use std::sync::Arc;

use bitrouter_core::routers::routing_table::RoutingTable;
use warp::Filter;

/// Creates a warp filter for `GET /v1/routes`.
pub fn routes_filter<T>(
    table: Arc<T>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
{
    warp::path!("v1" / "routes")
        .and(warp::get())
        .and(warp::any().map(move || table.clone()))
        .map(handle_list_routes)
}

fn handle_list_routes<T: RoutingTable>(table: Arc<T>) -> impl warp::Reply {
    let entries = table.list_routes();
    let routes: Vec<serde_json::Value> = entries
        .into_iter()
        .map(|e| {
            serde_json::json!({
                "model": e.model,
                "provider": e.provider,
                "protocol": e.protocol,
            })
        })
        .collect();
    warp::reply::json(&serde_json::json!({ "routes": routes }))
}
