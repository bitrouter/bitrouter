//! P2P language model that tunnels requests to a remote BitRouter peer
//! over iroh QUIC.
//!
//! The model serializes `LanguageModelCallOptions` into an OpenAI-compatible
//! JSON body, sends it as a `TunnelRequest` to the remote peer, and
//! deserializes the response. The remote peer forwards the request to its
//! local warp server which handles all protocol conversion and routing.

use std::collections::HashMap;

use bitrouter_core::{
    api::openai::chat::types::ChatCompletionResponse,
    errors::{BitrouterError, Result},
    models::{
        language::{
            call_options::LanguageModelCallOptions,
            generate_result::LanguageModelGenerateResult,
            language_model::LanguageModel,
            stream_part::LanguageModelStreamPart,
            stream_result::{
                LanguageModelStreamResult, LanguageModelStreamResultRequest,
                LanguageModelStreamResultResponse,
            },
        },
        shared::types::JsonValue,
    },
};
use regex::Regex;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;

use bitrouter_p2p::{
    client::{TunnelResponse, send_request},
    frame::TunnelRequest,
};

use crate::openai::chat::api::{
    ByteStream, build_chat_request, drive_sse_stream, response_to_generate_result,
};

const P2P_PROVIDER_NAME: &str = "p2p";

/// A language model that tunnels requests to a remote BitRouter peer via
/// iroh QUIC.
#[derive(Clone)]
pub struct P2pModel {
    model_id: String,
    node_id: iroh::EndpointId,
    endpoint: iroh::Endpoint,
}

impl P2pModel {
    pub fn new(
        model_id: impl Into<String>,
        node_id: iroh::EndpointId,
        endpoint: iroh::Endpoint,
    ) -> Self {
        Self {
            model_id: model_id.into(),
            node_id,
            endpoint,
        }
    }

    /// Build a TunnelRequest from LanguageModelCallOptions.
    ///
    /// Serializes as OpenAI chat completions format. The remote peer's
    /// inbound handler forwards to its local warp server at
    /// `/v1/chat/completions`, which handles routing and protocol
    /// conversion.
    fn build_tunnel_request(
        &self,
        options: &LanguageModelCallOptions,
        stream: bool,
    ) -> Result<(TunnelRequest, JsonValue)> {
        let request = build_chat_request(&self.model_id, options, stream)?;
        let request_body = serde_json::to_value(&request).map_err(|e| {
            BitrouterError::invalid_request(
                Some(P2P_PROVIDER_NAME),
                format!("failed to serialize P2P request: {e}"),
                None,
            )
        })?;
        let body_bytes = serde_json::to_vec(&request_body).map_err(|e| {
            BitrouterError::invalid_request(
                Some(P2P_PROVIDER_NAME),
                format!("failed to encode P2P request body: {e}"),
                None,
            )
        })?;

        let mut headers = HashMap::new();
        headers.insert("content-type".to_owned(), "application/json".to_owned());

        let tunnel_req = TunnelRequest {
            method: "POST".to_owned(),
            path: "/v1/chat/completions".to_owned(),
            headers,
            body: body_bytes,
        };

        Ok((tunnel_req, request_body))
    }
}

impl LanguageModel for P2pModel {
    fn provider_name(&self) -> &str {
        P2P_PROVIDER_NAME
    }

    fn model_id(&self) -> &str {
        &self.model_id
    }

    async fn supported_urls(&self) -> bitrouter_core::models::shared::types::Record<String, Regex> {
        HashMap::new()
    }

    async fn generate(
        &self,
        options: LanguageModelCallOptions,
    ) -> Result<LanguageModelGenerateResult> {
        let (tunnel_req, request_body) = self.build_tunnel_request(&options, false)?;

        let response = send_request(&self.endpoint, self.node_id, tunnel_req).await?;

        match response {
            TunnelResponse::Complete { status, body, .. } => {
                if status >= 400 {
                    let body_str = String::from_utf8_lossy(&body).into_owned();
                    return Err(BitrouterError::provider_error(
                        P2P_PROVIDER_NAME,
                        format!("remote peer returned HTTP {status}"),
                        bitrouter_core::errors::ProviderErrorContext {
                            status_code: Some(status),
                            error_type: None,
                            code: None,
                            param: None,
                            request_id: None,
                            body: serde_json::from_str(&body_str).ok(),
                        },
                    ));
                }

                let response_json: JsonValue = serde_json::from_slice(&body).map_err(|e| {
                    BitrouterError::response_decode(
                        Some(P2P_PROVIDER_NAME),
                        format!("failed to decode P2P response: {e}"),
                        None,
                    )
                })?;

                let completion: ChatCompletionResponse =
                    serde_json::from_value(response_json.clone()).map_err(|e| {
                        BitrouterError::response_decode(
                            Some(P2P_PROVIDER_NAME),
                            format!("failed to parse P2P chat completion: {e}"),
                            Some(response_json.clone()),
                        )
                    })?;

                response_to_generate_result(completion, None, request_body, None, response_json)
            }
            TunnelResponse::Streaming { .. } => Err(BitrouterError::invalid_response(
                Some(P2P_PROVIDER_NAME),
                "expected non-streaming response but got streaming",
                None,
            )),
        }
    }

    async fn stream(&self, options: LanguageModelCallOptions) -> Result<LanguageModelStreamResult> {
        let include_raw_chunks = options.include_raw_chunks.unwrap_or(false);
        let abort_signal = options.abort_signal.clone();
        let (tunnel_req, request_body) = self.build_tunnel_request(&options, true)?;

        let response = send_request(&self.endpoint, self.node_id, tunnel_req).await?;

        match response {
            TunnelResponse::Streaming {
                status,
                body_stream,
                ..
            } => {
                if status >= 400 {
                    return Err(BitrouterError::provider_error(
                        P2P_PROVIDER_NAME,
                        format!("remote peer returned HTTP {status}"),
                        bitrouter_core::errors::ProviderErrorContext {
                            status_code: Some(status),
                            error_type: None,
                            code: None,
                            param: None,
                            request_id: None,
                            body: None,
                        },
                    ));
                }

                // Wrap the P2P byte stream as a ByteStream compatible with
                // the existing SSE parser.
                let byte_stream: ByteStream =
                    Box::pin(tokio_stream::StreamExt::map(body_stream, |r| {
                        r.map_err(|e| Box::new(e) as Box<dyn std::error::Error + Send + Sync>)
                    }));

                let (sender, receiver) = mpsc::channel::<LanguageModelStreamPart>(32);
                tokio::spawn(drive_sse_stream(
                    byte_stream,
                    abort_signal,
                    sender,
                    include_raw_chunks,
                ));
                let stream = Box::pin(ReceiverStream::new(receiver));

                Ok(LanguageModelStreamResult {
                    stream,
                    request: Some(LanguageModelStreamResultRequest {
                        headers: None,
                        body: Some(request_body),
                    }),
                    response: Some(LanguageModelStreamResultResponse { headers: None }),
                })
            }
            TunnelResponse::Complete { body, .. } => {
                // Fallback: if we expected streaming but got complete,
                // try to parse as a non-streaming response. This can
                // happen if the remote returned an error.
                let body_str = String::from_utf8_lossy(&body).into_owned();
                Err(BitrouterError::invalid_response(
                    Some(P2P_PROVIDER_NAME),
                    format!("expected streaming response but got complete: {body_str}"),
                    None,
                ))
            }
        }
    }
}
