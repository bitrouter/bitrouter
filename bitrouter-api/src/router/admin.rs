//! Warp filters for the admin route management API.
//!
//! Provides HTTP endpoints for managing routes at runtime without requiring
//! config file rewrites and daemon restarts:
//!
//! - `GET /admin/routes` — list all routes (config-defined + dynamic)
//! - `POST /admin/routes` — create or update a dynamic route
//! - `DELETE /admin/routes/:name` — remove a dynamically-added route

use std::sync::Arc;

use bitrouter_core::routers::admin::{AdminRoutingTable, DynamicRoute};
use serde::Serialize;
use warp::Filter;

/// Mount all admin route management endpoints under `/admin/routes`.
///
/// The caller is responsible for auth gating — this function does not apply
/// any authentication. Compose with an auth filter before mounting:
///
/// ```ignore
/// let admin = auth_gate(management_auth(ctx.clone()))
///     .and(admin::admin_routes_filter(table.clone()));
/// ```
pub fn admin_routes_filter<T>(
    table: Arc<T>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminRoutingTable + Send + Sync + 'static,
{
    list_routes(table.clone())
        .or(create_route(table.clone()))
        .or(delete_route(table))
}

// ── GET /admin/routes ────────────────────────────────────────────────

fn list_routes<T>(
    table: Arc<T>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminRoutingTable + Send + Sync + 'static,
{
    warp::path!("admin" / "routes")
        .and(warp::get())
        .and(warp::any().map(move || table.clone()))
        .map(handle_list_routes)
}

#[derive(Serialize)]
struct AdminRouteListEntry {
    model: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    strategy: Option<String>,
    endpoints: Vec<AdminRouteEndpoint>,
    source: &'static str,
}

#[derive(Serialize)]
struct AdminRouteEndpoint {
    provider: String,
    model_id: String,
}

fn handle_list_routes<T: AdminRoutingTable>(table: Arc<T>) -> impl warp::Reply {
    let mut entries: Vec<AdminRouteListEntry> = Vec::new();

    // Config-defined routes (from the inner table).
    for entry in table.list_routes() {
        entries.push(AdminRouteListEntry {
            model: entry.model,
            strategy: None,
            endpoints: vec![AdminRouteEndpoint {
                provider: entry.provider,
                model_id: String::new(),
            }],
            source: "config",
        });
    }

    // Dynamic routes.
    for route in table.list_dynamic_routes() {
        // Remove any config entry that is shadowed by a dynamic route.
        entries.retain(|e| e.model != route.model);

        let strategy = match route.strategy {
            bitrouter_core::routers::admin::RouteStrategy::Priority => "priority",
            bitrouter_core::routers::admin::RouteStrategy::LoadBalance => "load_balance",
        };
        entries.push(AdminRouteListEntry {
            model: route.model,
            strategy: Some(strategy.to_owned()),
            endpoints: route
                .endpoints
                .into_iter()
                .map(|ep| AdminRouteEndpoint {
                    provider: ep.provider,
                    model_id: ep.model_id,
                })
                .collect(),
            source: "dynamic",
        });
    }

    entries.sort_by(|a, b| a.model.cmp(&b.model));
    warp::reply::json(&serde_json::json!({ "routes": entries }))
}

// ── POST /admin/routes ───────────────────────────────────────────────

fn create_route<T>(
    table: Arc<T>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminRoutingTable + Send + Sync + 'static,
{
    warp::path!("admin" / "routes")
        .and(warp::post())
        .and(warp::body::json::<DynamicRoute>())
        .and(warp::any().map(move || table.clone()))
        .map(handle_create_route)
}

fn handle_create_route<T: AdminRoutingTable>(
    route: DynamicRoute,
    table: Arc<T>,
) -> impl warp::Reply {
    let model = route.model.clone();
    match table.add_route(route) {
        Ok(()) => warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "status": "ok",
                "model": model,
            })),
            warp::http::StatusCode::OK,
        ),
        Err(e) => warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": { "message": e.to_string() }
            })),
            warp::http::StatusCode::BAD_REQUEST,
        ),
    }
}

// ── DELETE /admin/routes/:name ───────────────────────────────────────

fn delete_route<T>(
    table: Arc<T>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AdminRoutingTable + Send + Sync + 'static,
{
    warp::path!("admin" / "routes" / String)
        .and(warp::delete())
        .and(warp::any().map(move || table.clone()))
        .map(handle_delete_route)
}

fn handle_delete_route<T: AdminRoutingTable>(name: String, table: Arc<T>) -> impl warp::Reply {
    match table.remove_route(&name) {
        Ok(true) => warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "status": "ok",
                "model": name,
                "removed": true,
            })),
            warp::http::StatusCode::OK,
        ),
        Ok(false) => warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": { "message": format!("no dynamic route found for model: {name}") }
            })),
            warp::http::StatusCode::NOT_FOUND,
        ),
        Err(e) => warp::reply::with_status(
            warp::reply::json(&serde_json::json!({
                "error": { "message": e.to_string() }
            })),
            warp::http::StatusCode::INTERNAL_SERVER_ERROR,
        ),
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_core::errors::{BitrouterError, Result};
    use bitrouter_core::routers::admin::{
        AdminRoutingTable, DynamicRoute, RouteEndpoint, RouteStrategy,
    };
    use bitrouter_core::routers::dynamic::DynamicRoutingTable;
    use bitrouter_core::routers::routing_table::{
        ApiProtocol, RouteEntry, RoutingTable, RoutingTarget,
    };

    use super::admin_routes_filter;

    struct MockTable;

    impl RoutingTable for MockTable {
        async fn route(&self, incoming: &str) -> Result<RoutingTarget> {
            if incoming == "default" {
                Ok(RoutingTarget {
                    provider_name: "openai".to_owned(),
                    model_id: "gpt-4o".to_owned(),
                })
            } else {
                Err(BitrouterError::invalid_request(
                    None,
                    format!("no route: {incoming}"),
                    None,
                ))
            }
        }

        fn list_routes(&self) -> Vec<RouteEntry> {
            vec![RouteEntry {
                model: "default".to_owned(),
                provider: "openai".to_owned(),
                protocol: ApiProtocol::Openai,
            }]
        }
    }

    fn test_table() -> Arc<DynamicRoutingTable<MockTable>> {
        Arc::new(DynamicRoutingTable::new(MockTable))
    }

    #[tokio::test]
    async fn list_routes_returns_config_routes() {
        let table = test_table();
        let filter = admin_routes_filter(table);

        let res = warp::test::request()
            .method("GET")
            .path("/admin/routes")
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        let routes = body["routes"].as_array().unwrap();
        assert_eq!(routes.len(), 1);
        assert_eq!(routes[0]["model"], "default");
        assert_eq!(routes[0]["source"], "config");
    }

    #[tokio::test]
    async fn create_route_success() {
        let table = test_table();
        let filter = admin_routes_filter(table.clone());

        let body = serde_json::json!({
            "model": "research",
            "strategy": "load_balance",
            "endpoints": [
                { "provider": "openai", "model_id": "gpt-4o" },
                { "provider": "anthropic", "model_id": "claude-sonnet-4-20250514" }
            ]
        });

        let res = warp::test::request()
            .method("POST")
            .path("/admin/routes")
            .json(&body)
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 200);
        let resp: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        assert_eq!(resp["status"], "ok");
        assert_eq!(resp["model"], "research");

        // Verify the route was added.
        assert_eq!(table.list_dynamic_routes().len(), 1);
    }

    #[tokio::test]
    async fn create_route_empty_endpoints_fails() {
        let table = test_table();
        let filter = admin_routes_filter(table);

        let body = serde_json::json!({
            "model": "empty",
            "endpoints": []
        });

        let res = warp::test::request()
            .method("POST")
            .path("/admin/routes")
            .json(&body)
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 400);
    }

    #[tokio::test]
    async fn delete_route_success() {
        let table = test_table();

        // First add a dynamic route.
        table
            .add_route(DynamicRoute {
                model: "temp".to_owned(),
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "openai".to_owned(),
                    model_id: "gpt-4o".to_owned(),
                }],
            })
            .ok();

        let filter = admin_routes_filter(table.clone());

        let res = warp::test::request()
            .method("DELETE")
            .path("/admin/routes/temp")
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 200);
        let resp: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        assert_eq!(resp["removed"], true);
        assert!(table.list_dynamic_routes().is_empty());
    }

    #[tokio::test]
    async fn delete_nonexistent_route_returns_404() {
        let table = test_table();
        let filter = admin_routes_filter(table);

        let res = warp::test::request()
            .method("DELETE")
            .path("/admin/routes/nope")
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 404);
    }

    #[tokio::test]
    async fn dynamic_route_shadows_config_in_listing() {
        let table = test_table();

        // Add a dynamic route that shadows "default".
        table
            .add_route(DynamicRoute {
                model: "default".to_owned(),
                strategy: RouteStrategy::Priority,
                endpoints: vec![RouteEndpoint {
                    provider: "anthropic".to_owned(),
                    model_id: "claude-sonnet-4-20250514".to_owned(),
                }],
            })
            .ok();

        let filter = admin_routes_filter(table);

        let res = warp::test::request()
            .method("GET")
            .path("/admin/routes")
            .reply(&filter)
            .await;

        assert_eq!(res.status(), 200);
        let body: serde_json::Value = serde_json::from_slice(res.body()).unwrap();
        let routes = body["routes"].as_array().unwrap();

        // Should have only 1 entry for "default" (the dynamic one).
        let default_routes: Vec<_> = routes.iter().filter(|r| r["model"] == "default").collect();
        assert_eq!(default_routes.len(), 1);
        assert_eq!(default_routes[0]["source"], "dynamic");
    }
}
