//! axum HTTP server — gated behind the `server` feature.
//!
//! Wires all four inbound protocols to the `language_model` pipeline:
//! - `POST /v1/messages` — Anthropic Messages
//! - `POST /v1/chat/completions` — OpenAI Chat Completions
//! - `POST /v1/responses` — OpenAI Responses
//! - `POST /v1beta/models/{model_action}` — Google `generateContent` /
//!   `streamGenerateContent`
//!
//! Each handler parses the inbound body with that protocol's adapter, runs the
//! pipeline, and renders the result back in the **same** inbound protocol —
//! the outbound (provider) protocol is chosen per routing target.

use std::convert::Infallible;
use std::sync::Arc;

use axum::Router;
use axum::body::Body;
use axum::extract::{Path, State};
use axum::http::{HeaderMap, StatusCode, header};
use axum::response::{IntoResponse, Response};
use axum::routing::{get, post};
use axum::{Json, serve};
use futures::StreamExt;

use crate::app::App;
use crate::caller::CallerContext;
use crate::error::{BitrouterError, Result};
use crate::language_model::Pipeline;
use crate::language_model::protocol::{adapter_for, sanitize_model_name};
use crate::language_model::stream::{SseFrame, SseKeepaliveStream};
use crate::language_model::types::{ApiProtocol, PipelineRequest};

/// Shared axum state.
#[derive(Clone)]
pub struct AppState {
    /// The `language_model` pipeline.
    pub language_model: Arc<Pipeline>,
    /// SDK-level `skip_auth`: when `true`, a credential-less request is given a
    /// synthesised local caller; otherwise a pre-auth anonymous placeholder
    /// (an `AuthHook` is then expected to validate / reject).
    pub skip_auth: bool,
}

impl App {
    /// Serve this app's HTTP API on `listen` (e.g. `"0.0.0.0:4356"`).
    pub async fn serve(&self, listen: &str) -> Result<()> {
        let pipeline = self
            .language_model()
            .ok_or_else(|| {
                BitrouterError::internal("App::serve: no language_model pipeline configured")
            })?
            .clone();
        let state = AppState {
            language_model: pipeline.clone(),
            skip_auth: self.skip_auth(),
        };
        let router = build_router(state);
        let listener = tokio::net::TcpListener::bind(listen)
            .await
            .map_err(|e| BitrouterError::internal(format!("bind {listen}: {e}")))?;
        tracing::info!(%listen, "bitrouter listening");
        // Graceful shutdown (007 §1.2 / §6.2 / 008 §3.6): on SIGINT/SIGTERM
        // stop accepting new connections and let in-flight requests finish.
        let drain_pipeline = pipeline.clone();
        serve(listener, router)
            .with_graceful_shutdown(shutdown_signal())
            .await
            .map_err(|e| BitrouterError::internal(format!("serve: {e}")))?;
        // After the HTTP server drains, also wait for every detached client-
        // disconnect settlement task (StreamSettlementGuard::drop). Without
        // this, SIGTERM during heavy streaming traffic could drop the
        // detached tasks mid-await and lose receipts (008 §3.5).
        let drained = drain_pipeline.drain_pending_settlements().await;
        if drained > 0 {
            tracing::info!(drained, "drained pending streaming settlements on shutdown");
        }
        Ok(())
    }
}

/// Resolves when the process receives `SIGINT` (Ctrl-C) or `SIGTERM`.
async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        match tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate()) {
            Ok(mut sig) => {
                sig.recv().await;
            }
            Err(_) => std::future::pending::<()>().await,
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();

    tokio::select! {
        _ = ctrl_c => {}
        _ = terminate => {}
    }
    tracing::info!("shutdown signal received — draining in-flight requests");
}

/// Inbound request body ceiling. LLM prompts can be large (long context, image
/// data-URLs), so the limit is generous — but bounded, so a request body can
/// never be an unbounded allocation.
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;

/// Build the axum router for the given state.
pub fn build_router(state: AppState) -> Router {
    Router::new()
        .route("/v1/messages", post(anthropic_messages))
        .route("/v1/chat/completions", post(openai_chat))
        .route("/v1/responses", post(openai_responses))
        .route("/v1beta/models/{model_action}", post(google_generate))
        .route("/v1/models", get(list_models))
        .route("/health", get(health))
        .layer(axum::extract::DefaultBodyLimit::max(MAX_BODY_BYTES))
        .with_state(state)
}

async fn health() -> impl IntoResponse {
    (StatusCode::OK, Json(serde_json::json!({ "status": "ok" })))
}

async fn list_models(State(state): State<AppState>) -> impl IntoResponse {
    let models = state.language_model.routing_table().list_models();
    let data: Vec<_> = models
        .into_iter()
        .map(|m| serde_json::json!({ "id": m.id, "object": "model", "providers": m.providers }))
        .collect();
    Json(serde_json::json!({ "object": "list", "data": data }))
}

// ===== inbound protocol handlers =====

async fn anthropic_messages(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    handle(state, headers, ApiProtocol::Anthropic, body, None).await
}

async fn openai_chat(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    handle(state, headers, ApiProtocol::Openai, body, None).await
}

async fn openai_responses(
    State(state): State<AppState>,
    headers: HeaderMap,
    Json(body): Json<serde_json::Value>,
) -> Response {
    handle(state, headers, ApiProtocol::Responses, body, None).await
}

/// Google encodes the model and the streaming verb in the path segment, e.g.
/// `gemini-2.0-flash:generateContent` or `…:streamGenerateContent`.
async fn google_generate(
    State(state): State<AppState>,
    Path(model_action): Path<String>,
    headers: HeaderMap,
    Json(mut body): Json<serde_json::Value>,
) -> Response {
    let (model, action) = match model_action.rsplit_once(':') {
        Some((m, a)) => (m.to_string(), a.to_string()),
        None => {
            return BitrouterError::bad_request(
                "google path must be 'models/{model}:generateContent'",
            )
            .into_response();
        }
    };
    // Google carries the model in the URL, not the body — inject it so the
    // adapter sees it, and set the stream flag from the verb.
    if let Some(obj) = body.as_object_mut() {
        obj.insert("model".into(), model.clone().into());
        obj.insert("stream".into(), (action == "streamGenerateContent").into());
    }
    handle(state, headers, ApiProtocol::Google, body, Some(model)).await
}

/// Shared handler: parse with the inbound adapter, run the pipeline, render the
/// reply back in the same inbound protocol.
async fn handle(
    state: AppState,
    headers: HeaderMap,
    inbound: ApiProtocol,
    body: serde_json::Value,
    model_override: Option<String>,
) -> Response {
    let adapter = adapter_for(inbound);
    let prompt = match adapter.parse_request(body) {
        Ok(mut p) => {
            if let Some(model) = model_override {
                p.model = model;
            }
            p.model = sanitize_model_name(&p.model);
            p
        }
        Err(e) => return e.into_response(),
    };

    // `skip_auth` decides the starting caller: a synthesised local caller when
    // on, else a pre-auth anonymous placeholder for `AuthHook` to upgrade or
    // reject (003 §10 / 004 §3.4).
    let caller = if state.skip_auth {
        CallerContext::local()
    } else {
        CallerContext::anonymous()
    };
    let mut req = PipelineRequest::new(prompt.model.clone(), caller, prompt.clone());
    req.headers = headers;

    if prompt.stream {
        stream_response(state.language_model.clone(), req, inbound, &prompt.model)
    } else {
        match state.language_model.execute(req).await {
            Ok(resp) => match adapter.render_response(&resp.result, &prompt, &resp.request_id) {
                Ok(json) => Json(json).into_response(),
                Err(e) => e.into_response(),
            },
            Err(e) => e.into_response(),
        }
    }
}

/// Build a `text/event-stream` response: pipe the canonical part stream through
/// the inbound protocol's `StreamEncoder`, wrap it in `SseKeepaliveStream`, and
/// stream the wire bytes.
fn stream_response(
    pipeline: Arc<Pipeline>,
    req: PipelineRequest,
    inbound: ApiProtocol,
    model: &str,
) -> Response {
    let adapter = adapter_for(inbound);
    let mut encoder = adapter.stream_encoder(&req.request_id, model);
    let keepalive = pipeline.keepalive_interval();

    let frame_stream = async_stream::stream! {
        match pipeline.execute_stream(req).await {
            Ok(mut parts) => {
                while let Some(item) = parts.next().await {
                    match item {
                        Ok(part) => {
                            if let Ok(frames) = encoder.encode(&part) {
                                for f in frames {
                                    yield f;
                                }
                            }
                        }
                        Err(e) => {
                            // Surface the error as a protocol-shaped terminal
                            // error event the client will actually recognise,
                            // then stop.
                            for f in encoder.encode_error(&e.to_string()) {
                                yield f;
                            }
                            return;
                        }
                    }
                }
                if let Ok(frames) = encoder.finish() {
                    for f in frames {
                        yield f;
                    }
                }
            }
            Err(e) => {
                for f in encoder.encode_error(&e.to_string()) {
                    yield f;
                }
            }
        }
    };

    let with_keepalive = SseKeepaliveStream::new(frame_stream, keepalive);
    let byte_stream =
        with_keepalive.map(|frame: SseFrame| Ok::<_, Infallible>(frame.to_wire().into_bytes()));

    Response::builder()
        .status(StatusCode::OK)
        .header(header::CONTENT_TYPE, "text/event-stream")
        .header(header::CACHE_CONTROL, "no-cache")
        .body(Body::from_stream(byte_stream))
        .unwrap_or_else(|e| {
            BitrouterError::internal(format!("building stream response: {e}")).into_response()
        })
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
        // 003 §6.4 — payment / rate-limit responses must carry the headers
        // that auto-paying clients (e.g. the MPP autopay flow, 004 §3.3) and
        // well-behaved API consumers expect. RFC 7235 §4.1 for
        // WWW-Authenticate, RFC 7231 §7.1.3 for Retry-After.
        let mut response = (status, body).into_response();
        match &self {
            BitrouterError::PaymentRequired(_) => {
                if let Ok(v) = header::HeaderValue::from_str(
                    "Bitrouter-MPP realm=\"bitrouter\", scheme=\"tempo-voucher\"",
                ) {
                    response.headers_mut().insert(header::WWW_AUTHENTICATE, v);
                }
            }
            BitrouterError::RateLimited {
                retry_after: Some(secs),
            } => {
                if let Ok(v) = header::HeaderValue::from_str(&secs.to_string()) {
                    response.headers_mut().insert(header::RETRY_AFTER, v);
                }
            }
            _ => {}
        }
        response
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn payment_required_emits_www_authenticate() {
        let response =
            BitrouterError::PaymentRequired("send a Tempo voucher".to_string()).into_response();
        assert_eq!(response.status().as_u16(), 402);
        let www_auth = response
            .headers()
            .get(header::WWW_AUTHENTICATE)
            .expect("402 must carry WWW-Authenticate (003 §6.4)")
            .to_str()
            .unwrap();
        assert!(www_auth.contains("Bitrouter-MPP"));
        assert!(www_auth.contains("tempo-voucher"));
    }

    #[test]
    fn rate_limited_emits_retry_after_when_present() {
        let response = BitrouterError::RateLimited {
            retry_after: Some(42),
        }
        .into_response();
        assert_eq!(response.status().as_u16(), 429);
        let retry = response
            .headers()
            .get(header::RETRY_AFTER)
            .expect("429 with retry_after must carry Retry-After")
            .to_str()
            .unwrap();
        assert_eq!(retry, "42");
    }

    #[test]
    fn rate_limited_omits_retry_after_when_unknown() {
        let response = BitrouterError::RateLimited { retry_after: None }.into_response();
        assert_eq!(response.status().as_u16(), 429);
        assert!(
            response.headers().get(header::RETRY_AFTER).is_none(),
            "no Retry-After when the daemon doesn't know how long to wait"
        );
    }
}
