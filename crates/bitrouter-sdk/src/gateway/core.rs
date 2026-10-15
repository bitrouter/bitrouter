use bitrouter_ai::core::protocol::Capabilities;

use super::GatewayClient;
use crate::error::{Error, Result};
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
        let response =
            transport::successful_as(response, self.options.max_response_bytes, Error::Core)
                .await?;
        if !transport::has_content_type(&response, "application/json") {
            return Err(Error::Response("expected application/json"));
        }
        serde_json::from_slice(&transport::body(response, self.options.max_response_bytes).await?)
            .map_err(|_| Error::Response("body does not match core capabilities"))
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
