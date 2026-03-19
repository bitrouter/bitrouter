//! A2A agent discovery endpoints.
//!
//! Provides `GET /.well-known/agent-card.json` and `GET /a2a/agents`.

mod handler;

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
        .and(warp::query::<handler::WellKnownQuery>())
        .and(warp::any().map(move || registry.clone()))
        .map(handler::handle_well_known)
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
        .map(handler::handle_agent_list)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::router::a2a::test_helpers::*;

    use bitrouter_a2a::card::minimal_card;
    use bitrouter_a2a::registry::AgentRegistration;

    #[tokio::test]
    async fn well_known_returns_404_when_empty() {
        let registry = setup_empty_registry();
        let filter = well_known_filter(registry);

        let resp = warp::test::request()
            .path("/.well-known/agent-card.json")
            .reply(&filter)
            .await;

        assert_eq!(resp.status(), 404);
    }

    #[tokio::test]
    async fn well_known_returns_card() {
        let registry = setup_empty_registry();

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
        let registry = setup_empty_registry();

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
        let registry = setup_empty_registry();

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
