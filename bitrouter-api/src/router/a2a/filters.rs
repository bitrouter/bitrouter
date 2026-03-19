//! Warp HTTP filters for A2A agent discovery.
//!
//! Provides the standard `/.well-known/agent-card.json` discovery endpoint
//! and a `/a2a/agents` listing endpoint, per the A2A v1.0 specification.

use std::sync::Arc;

use warp::Filter;

use bitrouter_a2a::registry::AgentCardRegistry;

/// Creates a warp filter for `GET /.well-known/agent-card.json`.
///
/// Returns the first registered agent card (alphabetically), or a specific
/// agent when `?name=<agent_name>` is provided. Includes `Cache-Control`
/// and `ETag` headers per the A2A v1.0 discovery specification.
pub fn well_known_filter<R>(
    registry: Arc<R>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    R: AgentCardRegistry + 'static,
{
    warp::path!(".well-known" / "agent-card.json")
        .and(warp::get())
        .and(warp::query::<WellKnownQuery>())
        .and(warp::any().map(move || registry.clone()))
        .map(handle_well_known)
}

/// Creates a warp filter for `GET /a2a/agents`.
///
/// Returns a JSON array of all registered agent cards. The `iss` binding
/// is stripped from the public response.
pub fn agent_list_filter<R>(
    registry: Arc<R>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    R: AgentCardRegistry + 'static,
{
    warp::path!("a2a" / "agents")
        .and(warp::get())
        .and(warp::any().map(move || registry.clone()))
        .map(handle_agent_list)
}

#[derive(Debug, serde::Deserialize)]
struct WellKnownQuery {
    name: Option<String>,
}

fn handle_well_known<R: AgentCardRegistry>(
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

fn handle_agent_list<R: AgentCardRegistry>(registry: Arc<R>) -> Box<dyn warp::Reply> {
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

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_a2a::card::minimal_card;
    use bitrouter_a2a::file_registry::FileAgentCardRegistry;
    use bitrouter_a2a::registry::AgentRegistration;

    fn setup_registry(dir: &std::path::Path) -> Arc<FileAgentCardRegistry> {
        Arc::new(FileAgentCardRegistry::new(dir).expect("new registry"))
    }

    #[tokio::test]
    async fn well_known_returns_404_when_empty() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());
        let filter = well_known_filter(registry);

        let resp = warp::test::request()
            .path("/.well-known/agent-card.json")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn well_known_returns_card() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());

        registry
            .register(AgentRegistration {
                card: minimal_card("test-agent", "A test", "1.0.0", "http://localhost:8787"),
                iss: Some("solana:test:key".to_string()),
            })
            .expect("register");

        let filter = well_known_filter(registry);
        let resp = warp::test::request()
            .path("/.well-known/agent-card.json")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        assert_eq!(
            resp.headers().get("Cache-Control").expect("cache-control"),
            "max-age=3600"
        );
        assert_eq!(resp.headers().get("ETag").expect("etag"), "\"1.0.0\"");

        // Verify iss is NOT in the response (card only, not registration).
        let body = String::from_utf8_lossy(resp.body());
        assert!(!body.contains("solana:test:key"));
        assert!(body.contains("test-agent"));
    }

    #[tokio::test]
    async fn well_known_with_name_query() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());

        registry
            .register(AgentRegistration {
                card: minimal_card("alpha", "Agent A", "1.0.0", "http://localhost:8787"),
                iss: None,
            })
            .expect("register");
        registry
            .register(AgentRegistration {
                card: minimal_card("beta", "Agent B", "2.0.0", "http://localhost:8787"),
                iss: None,
            })
            .expect("register");

        let filter = well_known_filter(registry);

        let resp = warp::test::request()
            .path("/.well-known/agent-card.json?name=beta")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let body = String::from_utf8_lossy(resp.body());
        assert!(body.contains("beta"));
        assert!(body.contains("Agent B"));
    }

    #[tokio::test]
    async fn agent_list_returns_all_cards() {
        let dir = tempfile::tempdir().expect("tempdir");
        let registry = setup_registry(dir.path());

        registry
            .register(AgentRegistration {
                card: minimal_card("alpha", "Agent A", "1.0.0", "http://localhost:8787"),
                iss: Some("secret-iss".to_string()),
            })
            .expect("register");
        registry
            .register(AgentRegistration {
                card: minimal_card("beta", "Agent B", "1.0.0", "http://localhost:8787"),
                iss: None,
            })
            .expect("register");

        let filter = agent_list_filter(registry);
        let resp = warp::test::request()
            .path("/a2a/agents")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 200);
        let body = String::from_utf8_lossy(resp.body());
        assert!(body.contains("alpha"));
        assert!(body.contains("beta"));
        // iss should be stripped.
        assert!(!body.contains("secret-iss"));
    }
}
