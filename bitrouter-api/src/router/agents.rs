//! Warp filter for the `GET /v1/agents` endpoint.
//!
//! Returns all agents available through the router, including
//! metadata such as name, description, version, skills, and
//! supported content modes.
//!
//! Supports optional query parameter filters:
//!
//! - `provider` — exact match on provider name
//! - `id` — substring match on agent ID (case-insensitive)
//! - `skill` — agent must have a skill whose name contains this substring

use std::sync::Arc;

use bitrouter_core::routers::registry::AgentRegistry;
use serde::Serialize;
use warp::Filter;

/// Query parameters for filtering the agent list.
#[derive(Debug, Default)]
struct AgentQuery {
    /// Filter by provider name (exact match).
    provider: Option<String>,
    /// Filter by agent ID (substring match, case-insensitive).
    id: Option<String>,
    /// Filter by skill name (substring match, case-insensitive).
    skill: Option<String>,
}

/// Creates a warp filter for `GET /v1/agents`.
///
/// Accepts `Option<Arc<T>>` — when `None` (no agent source configured), the
/// endpoint returns 404.
pub fn agents_filter<T>(
    registry: Option<Arc<T>>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: AgentRegistry + Send + Sync + 'static,
{
    warp::path!("v1" / "agents")
        .and(warp::get())
        .and(optional_raw_query())
        .and(warp::any().map(move || registry.clone()))
        .and_then(handle_list_agents)
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
struct AgentResponse {
    id: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    version: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    skills: Vec<SkillResponse>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    input_modes: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    output_modes: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    streaming: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    icon_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    documentation_url: Option<String>,
}

#[derive(Serialize)]
struct SkillResponse {
    id: String,
    name: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    tags: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    examples: Vec<String>,
}

fn parse_query(raw: &str) -> AgentQuery {
    let mut query = AgentQuery::default();
    for pair in raw.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "provider" => query.provider = Some(value.to_owned()),
                "id" => query.id = Some(value.to_owned()),
                "skill" => query.skill = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    query
}

async fn handle_list_agents<T: AgentRegistry + Send + Sync>(
    raw_query: Option<String>,
    registry: Option<Arc<T>>,
) -> Result<impl warp::Reply, warp::Rejection> {
    let Some(registry) = registry else {
        return Err(warp::reject::not_found());
    };
    let query = raw_query.as_deref().map(parse_query).unwrap_or_default();
    let entries = registry.list_agents().await;
    let id_lower = query.id.as_deref().map(str::to_lowercase);
    let skill_lower = query.skill.as_deref().map(str::to_lowercase);

    let agents: Vec<AgentResponse> = entries
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
            if skill_lower
                .as_deref()
                .is_some_and(|s| !e.skills.iter().any(|sk| sk.name.to_lowercase().contains(s)))
            {
                return false;
            }
            true
        })
        .map(|e| AgentResponse {
            id: e.id,
            name: e.name,
            provider: e.provider,
            description: e.description,
            version: e.version,
            skills: e
                .skills
                .into_iter()
                .map(|s| SkillResponse {
                    id: s.id,
                    name: s.name,
                    description: s.description,
                    tags: s.tags,
                    examples: s.examples,
                })
                .collect(),
            input_modes: e.input_modes,
            output_modes: e.output_modes,
            streaming: e.streaming,
            icon_url: e.icon_url,
            documentation_url: e.documentation_url,
        })
        .collect();
    Ok(warp::reply::json(&serde_json::json!({ "agents": agents })))
}
