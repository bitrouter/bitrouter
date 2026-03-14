//! Warp filter for the `GET /v1/models` endpoint.
//!
//! Returns all models available across all configured providers, including
//! metadata such as display name, description, context window, modalities,
//! and the owning provider.
//!
//! Supports optional query parameter filters:
//!
//! - `provider` — exact match on provider name
//! - `id` — substring match on model ID
//! - `input_modality` — model must support this input modality
//! - `output_modality` — model must support this output modality

use std::sync::Arc;

use bitrouter_core::routers::routing_table::RoutingTable;
use serde::Serialize;
use warp::Filter;

/// Query parameters for filtering the model list.
#[derive(Debug, Default)]
pub struct ModelQuery {
    /// Filter by provider name (exact match).
    pub provider: Option<String>,
    /// Filter by model ID (substring match, case-insensitive).
    pub id: Option<String>,
    /// Filter by supported input modality (e.g. "text", "image").
    pub input_modality: Option<String>,
    /// Filter by supported output modality.
    pub output_modality: Option<String>,
}

/// Creates a warp filter for `GET /v1/models`.
pub fn models_filter<T>(
    table: Arc<T>,
) -> impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone
where
    T: RoutingTable + Send + Sync + 'static,
{
    warp::path!("v1" / "models")
        .and(warp::get())
        .and(optional_raw_query())
        .and(warp::any().map(move || table.clone()))
        .map(handle_list_models)
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
struct ModelResponse {
    id: String,
    provider: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    description: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_input_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    max_output_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    input_modalities: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    output_modalities: Vec<String>,
}

fn parse_query(raw: &str) -> ModelQuery {
    let mut query = ModelQuery::default();
    for pair in raw.split('&') {
        if let Some((key, value)) = pair.split_once('=') {
            match key {
                "provider" => query.provider = Some(value.to_owned()),
                "id" => query.id = Some(value.to_owned()),
                "input_modality" => query.input_modality = Some(value.to_owned()),
                "output_modality" => query.output_modality = Some(value.to_owned()),
                _ => {}
            }
        }
    }
    query
}

fn handle_list_models<T: RoutingTable>(
    raw_query: Option<String>,
    table: Arc<T>,
) -> impl warp::Reply {
    let query = raw_query.as_deref().map(parse_query).unwrap_or_default();
    let entries = table.list_models();
    let id_lower = query.id.as_deref().map(str::to_lowercase);

    let models: Vec<ModelResponse> = entries
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
            if query
                .input_modality
                .as_deref()
                .is_some_and(|m| !e.input_modalities.iter().any(|x| x == m))
            {
                return false;
            }
            if query
                .output_modality
                .as_deref()
                .is_some_and(|m| !e.output_modalities.iter().any(|x| x == m))
            {
                return false;
            }
            true
        })
        .map(|e| ModelResponse {
            id: e.id,
            provider: e.provider,
            name: e.name,
            description: e.description,
            max_input_tokens: e.max_input_tokens,
            max_output_tokens: e.max_output_tokens,
            input_modalities: e.input_modalities,
            output_modalities: e.output_modalities,
        })
        .collect();
    warp::reply::json(&serde_json::json!({ "models": models }))
}
