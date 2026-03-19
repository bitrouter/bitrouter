//! Handlers for A2A agent discovery endpoints.

use std::sync::Arc;

use bitrouter_a2a::registry::AgentCardRegistry;

#[derive(Debug, serde::Deserialize)]
pub(crate) struct WellKnownQuery {
    pub name: Option<String>,
}

pub(crate) fn handle_well_known<R: AgentCardRegistry>(
    query: WellKnownQuery,
    registry: Arc<R>,
) -> Box<dyn warp::Reply> {
    let result = if let Some(name) = &query.name {
        registry.get(name)
    } else {
        // Return the first agent alphabetically.
        registry.list().map(|mut list| list.pop())
    };

    match result {
        Ok(Some(reg)) => {
            let etag = format!("\"{}\"", reg.card.version);
            let json = warp::reply::json(&reg.card);
            let reply = warp::reply::with_header(json, "Cache-Control", "max-age=3600");
            let reply = warp::reply::with_header(reply, "ETag", etag);
            Box::new(reply)
        }
        Ok(None) => {
            let json = warp::reply::json(&serde_json::json!({
                "error": "no agent cards registered"
            }));
            Box::new(warp::reply::with_status(
                json,
                warp::http::StatusCode::NOT_FOUND,
            ))
        }
        Err(e) => {
            let json = warp::reply::json(&serde_json::json!({
                "error": e.to_string()
            }));
            Box::new(warp::reply::with_status(
                json,
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}

pub(crate) fn handle_agent_list<R: AgentCardRegistry>(registry: Arc<R>) -> Box<dyn warp::Reply> {
    match registry.list() {
        Ok(registrations) => {
            // Strip iss from public response — only expose the cards.
            let cards: Vec<_> = registrations.into_iter().map(|r| r.card).collect();
            Box::new(warp::reply::json(&serde_json::json!({ "agents": cards })))
        }
        Err(e) => {
            let json = warp::reply::json(&serde_json::json!({
                "error": e.to_string()
            }));
            Box::new(warp::reply::with_status(
                json,
                warp::http::StatusCode::INTERNAL_SERVER_ERROR,
            ))
        }
    }
}
