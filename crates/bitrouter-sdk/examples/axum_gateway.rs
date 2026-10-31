//! A loopback gateway composed from public contracts and the HTTP SDK only.
//!
//! Run with BITROUTER_URL and optionally BITROUTER_API_KEY. The example binds
//! 127.0.0.1:4357. Add application-owned identity, authorization and middleware
//! before exposing a deployment. No internal request headers are forwarded.

use axum::{Router, extract::State, routing::post};
use bitrouter_ai::{
    axum::{error::Error, extract::Json, response::Reply},
    decisions::DecisionRequest,
    protocol::{
        chat_completions::ChatRequest, messages::MessagesRequest, responses::ResponsesRequest,
    },
    wire::{DEFAULT_EVENT_LIMIT, DEFAULT_JSON_LIMIT},
};
use bitrouter_sdk::{
    error::Error as SdkError,
    gateway::{GatewayClient, reply::Body},
};
use futures_util::TryStreamExt;

// Select safe public presentation here; do not serialize SDK diagnostics.
fn public_error(_: SdkError) -> Error {
    Error::bad_gateway()
}

async fn chat(
    State(client): State<GatewayClient>,
    Json(request): Json<ChatRequest>,
) -> Result<Reply, Error> {
    let reply = client.chat_reply(&request).await.map_err(public_error)?;
    // Applications can record reply.metadata before returning its body.
    match reply.body {
        Body::Json(value) => {
            Reply::json(&value, DEFAULT_JSON_LIMIT).map_err(|_| Error::bad_gateway())
        }
        Body::Stream(stream) => Ok(Reply::chat_stream(
            stream.map_err(public_error),
            DEFAULT_EVENT_LIMIT,
        )),
    }
}

async fn messages(
    State(client): State<GatewayClient>,
    Json(request): Json<MessagesRequest>,
) -> Result<Reply, Error> {
    match client
        .messages_reply(&request)
        .await
        .map_err(public_error)?
        .body
    {
        Body::Json(value) => {
            Reply::json(&value, DEFAULT_JSON_LIMIT).map_err(|_| Error::bad_gateway())
        }
        Body::Stream(stream) => Ok(Reply::messages_stream(
            stream.map_err(public_error),
            DEFAULT_EVENT_LIMIT,
        )),
    }
}

async fn responses(
    State(client): State<GatewayClient>,
    Json(request): Json<ResponsesRequest>,
) -> Result<Reply, Error> {
    match client
        .responses_reply(&request)
        .await
        .map_err(public_error)?
        .body
    {
        Body::Json(value) => {
            Reply::json(&value, DEFAULT_JSON_LIMIT).map_err(|_| Error::bad_gateway())
        }
        Body::Stream(stream) => Ok(Reply::responses_stream(
            stream.map_err(public_error),
            DEFAULT_EVENT_LIMIT,
        )),
    }
}

async fn decisions(
    State(client): State<GatewayClient>,
    Json(request): Json<DecisionRequest>,
) -> Result<Reply, Error> {
    let response = client.decisions(&request).await.map_err(public_error)?;
    Reply::json(&response, DEFAULT_JSON_LIMIT).map_err(|_| Error::bad_gateway())
}

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let endpoint = std::env::var("BITROUTER_URL")?;
    let credential = std::env::var("BITROUTER_API_KEY").ok();
    let client = GatewayClient::new(&endpoint, credential.as_deref())?;
    let app = Router::new()
        .route("/v1/chat/completions", post(chat))
        .route("/v1/messages", post(messages))
        .route("/v1/responses", post(responses))
        .route("/v1/decisions", post(decisions))
        .with_state(client);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:4357").await?;
    axum::serve(listener, app).await?;
    Ok(())
}
