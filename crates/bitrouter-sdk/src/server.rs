//! axum HTTP server — gated behind the `server` feature.
//!
//! Phase 1 wires a single working path (`POST /v1/chat/completions`,
//! non-streaming) plus `/health`, so the pipeline is reachable end-to-end. The
//! full four-protocol inbound/outbound surface lands in Phase 2.

use std::sync::Arc;

use axum::Router;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, serve};

use crate::app::App;
use crate::caller::CallerContext;
use crate::error::BitrouterError;
use crate::language_model::{
    Content, GenerationParams, Message, Pipeline, PipelineRequest, Prompt, Role,
};

/// Shared axum state.
#[derive(Clone)]
pub struct AppState {
    /// The `language_model` pipeline.
    pub language_model: Arc<Pipeline>,
}

impl App {
    /// Serve this app's HTTP API on `listen` (e.g. `"0.0.0.0:4356"`).
    ///
    /// Requires the `language_model` protocol to have been configured.
    pub async fn serve(&self, listen: &str) -> crate::Result<()> {
        let pipeline = self
            .language_model()
            .ok_or_else(|| {
                BitrouterError::internal("App::serve: no language_model pipeline configured")
            })?
            .clone();
        let state = AppState {
            language_model: pipeline,
        };
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| BitrouterError::internal(format!("bind {listen}: {e}")))?;
        tracing::info!(%listen, "bitrouter listening");
        serve(listener, router)
            .await
            .map_err(|e| BitrouterError::internal(format!("serve: {e}")))?;
        Ok(())
    }
}

/// Build the axum router for the given state.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/chat/completions", post(chat_completions))
        .route("/health", get(health))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

/// Minimal OpenAI Chat Completions handler (non-streaming). Phase 2 replaces the
/// hand-rolled parsing with the real protocol adapter.
async fn chat_completions(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    match handle_chat(&state, headers, body).await {
        Ok(resp) => Json(resp).into_response(),
        Err(e) => e.into_response(),
    }
}

async fn handle_chat(
    state: &AppState,
    headers: HeaderMap,
    body: serde_json::Value,
) -> crate::Result<serde_json::Value> {
    let model = body
        .get("model")
        .and_then(|v| v.as_str())
        .ok_or_else(|| BitrouterError::bad_request("missing 'model'"))?
        .to_string();

    let messages = body
        .get("messages")
        .and_then(|v| v.as_array())
        .ok_or_else(|| BitrouterError::bad_request("missing 'messages'"))?;

    let mut parsed = Vec::with_capacity(messages.len());
    for m in messages {
        let role = match m.get("role").and_then(|v| v.as_str()) {
            Some("system") => Role::System,
            Some("user") => Role::User,
            Some("assistant") => Role::Assistant,
            Some("tool") => Role::Tool,
            other => {
                return Err(BitrouterError::bad_request(format!(
                    "unknown message role: {other:?}"
                )));
            }
        };
        let text = m
            .get("content")
            .and_then(|v| v.as_str())
            .unwrap_or_default()
            .to_string();
        parsed.push(Message::text(role, text));
    }

    let prompt = Prompt {
        model: model.clone(),
        system: None,
        messages: parsed,
        tools: Vec::new(),
        params: GenerationParams::default(),
        stream: false,
    };

    // Phase 1 uses a synthesised local caller; real auth arrives in Phase 3.
    let mut req = PipelineRequest::new(model, CallerContext::local(), prompt);
    req.headers = headers;

    let resp = state.language_model.execute(req).await?;

    let text: String = resp
        .result
        .content
        .iter()
        .filter_map(|c| match c {
            Content::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();

    Ok(serde_json::json!({
        "id": resp.request_id,
        "object": "chat.completion",
        "choices": [{
            "index": 0,
            "message": { "role": "assistant", "content": text },
            "finish_reason": "stop",
        }],
        "usage": resp.result.usage.map(|u| serde_json::json!({
            "prompt_tokens": u.prompt_tokens,
            "completion_tokens": u.completion_tokens,
            "total_tokens": u.total(),
        })),
    }))
}

impl IntoResponse for BitrouterError {
    fn into_response(self) -> Response {
        let status =
            StatusCode::from_u16(self.status()).unwrap_or(StatusCode::INTERNAL_SERVER_ERROR);
        let body = Json(serde_json::json!({
            "error": {
                "message": self.to_string(),
                "type": self.error_type(),
            }
        }));
        (status, body).into_response()
    }
}
