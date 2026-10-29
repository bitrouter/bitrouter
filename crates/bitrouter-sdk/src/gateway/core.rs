use bitrouter_ai::core::protocol::{BETA, Capabilities};
use bitrouter_ai::core::responses::CreateRequest;
use bitrouter_ai::protocol::responses::{ResponsesEvent, ResponsesResponse};
use reqwest::{Response, header};
use serde::de::DeserializeOwned;

use super::{GatewayClient, require_stream};
use crate::error::{Error, Result};
use crate::stream::{EventStream, Protocol};
use crate::transport;

impl GatewayClient {
    /// Read the existing managed-core HTTP capability surface. The service
    /// requires an active virtual key even when ordinary inference skips auth.
    pub async fn core_capabilities(&self) -> Result<Capabilities> {
        let response = self
            .http
            .get(self.url("v1/orchestrator/capabilities")?)
            .send()
            .await
            .map_err(Error::transport)?;
        self.core_json(response).await
    }

    /// Submit the managed Responses profile using its explicit beta negotiation.
    /// A separately bound core channel must remain available for caller-owned
    /// checkpoint persistence and ACKs. The SDK does not retry or execute tools.
    ///
    /// # Errors
    /// Returns request, transport, bounded decoding or typed core service errors.
    pub async fn managed_responses(&self, request: &CreateRequest) -> Result<ResponsesResponse> {
        require_stream(request.stream, false)?;
        let response = self
            .http
            .post(self.url("v1/responses")?)
            .header("bitrouter-beta", BETA)
            .json(request)
            .send()
            .await
            .map_err(Error::transport)?;
        self.core_json(response).await
    }

    /// Observe a managed response, including retained `bitrouter.*` events.
    /// Dropping the stream stops observation; it neither cancels the run nor
    /// acknowledges any checkpoint. HTTP/SSE access needs no SDK `core` feature.
    ///
    /// # Errors
    /// Returns request, transport or typed core HTTP errors. Stream items retain
    /// typed core failures, enforce byte bounds and reject missing terminal events.
    pub async fn stream_managed_responses(
        &self,
        request: &CreateRequest,
    ) -> Result<EventStream<ResponsesEvent>> {
        require_stream(request.stream, true)?;
        let response = self
            .http
            .post(self.url("v1/responses")?)
            .header("bitrouter-beta", BETA)
            .header(header::ACCEPT, "text/event-stream")
            .json(request)
            .send()
            .await
            .map_err(Error::transport)?;
        let response =
            transport::successful_as(response, self.options.max_response_bytes, Error::Core)
                .await?;
        if !transport::has_content_type(&response, "text/event-stream") {
            return Err(Error::Response("expected text/event-stream"));
        }
        Ok(crate::stream::decode(
            response,
            self.options.max_event_bytes,
            Protocol::ManagedResponses,
        ))
    }

    async fn core_json<T: DeserializeOwned>(&self, response: Response) -> Result<T> {
        let response =
            transport::successful_as(response, self.options.max_response_bytes, Error::Core)
                .await?;
        if !transport::has_content_type(&response, "application/json") {
            return Err(Error::Response("expected application/json"));
        }
        serde_json::from_slice(&transport::body(response, self.options.max_response_bytes).await?)
            .map_err(|_| Error::Response("body does not match the public core contract"))
    }

    /// Open the existing managed WebSocket channel using this client's HTTP
    /// origin, credentials, TLS, proxy and handshake timeout policies.
    ///
    /// Send an explicit bind/restore before the service's initial-message
    /// deadline. The SDK does not persist or ACK checkpoints, execute tools,
    /// reconnect, replay mutations or infer cancellation from disconnect.
    /// HTTP read/request deadlines apply to the handshake; callers control
    /// deadlines for subsequent channel operations.
    #[cfg(feature = "core")]
    pub async fn core_channel(
        &self,
        options: crate::core::ChannelOptions,
    ) -> Result<crate::core::CoreChannel> {
        crate::core::connect(
            self.channel_http.get(self.url("v1/orchestrator/channel")?),
            options,
            self.options.max_response_bytes,
        )
        .await
    }
}
