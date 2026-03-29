//! A2A gateway server traits.
//!
//! Provides the [`A2aDiscovery`] and [`A2aProxy`] traits for serving
//! downstream A2A clients by proxying to an upstream agent.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use tokio::sync::broadcast;

use crate::api::a2a::types::A2aGatewayError;
use crate::api::a2a::types::{
    AgentCard, GetTaskRequest, ListTasksRequest, ListTasksResponse, SendMessageRequest,
    StreamResponse, Task, TaskPushNotificationConfig,
};

/// Trait for agent card discovery.
///
/// Implementors cache the upstream agent's card and provide
/// change notification for downstream clients.
pub trait A2aDiscovery: Send + Sync {
    /// Get the cached agent card (with URL rewritten to gateway address).
    fn get_agent_card(&self) -> impl Future<Output = Option<AgentCard>> + Send;

    /// Subscribe to agent card change notifications.
    fn subscribe_card_changes(&self) -> broadcast::Receiver<()>;
}

/// Trait for proxying A2A protocol operations to an upstream agent.
///
/// Each method maps to an A2A v0.3.0 JSON-RPC method. Implementations
/// forward requests to the upstream agent and return responses.
pub trait A2aProxy: Send + Sync {
    /// Forward a `message/send` request.
    fn send_message(
        &self,
        request: SendMessageRequest,
    ) -> impl Future<Output = Result<StreamResponse, A2aGatewayError>> + Send;

    /// Forward a `tasks/get` request.
    fn get_task(
        &self,
        request: GetTaskRequest,
    ) -> impl Future<Output = Result<Task, A2aGatewayError>> + Send;

    /// Forward a `tasks/cancel` request.
    fn cancel_task(
        &self,
        request: crate::api::a2a::types::CancelTaskRequest,
    ) -> impl Future<Output = Result<Task, A2aGatewayError>> + Send;

    /// Forward a `tasks/list` request.
    fn list_tasks(
        &self,
        request: ListTasksRequest,
    ) -> impl Future<Output = Result<ListTasksResponse, A2aGatewayError>> + Send;

    /// Forward a `message/stream` request, returning a proxied SSE stream.
    fn send_streaming_message(
        &self,
        request: SendMessageRequest,
    ) -> impl Future<
        Output = Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError>,
    > + Send;

    /// Forward a `tasks/resubscribe` request, returning a proxied SSE stream.
    fn subscribe_to_task(
        &self,
        task_id: &str,
    ) -> impl Future<
        Output = Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send>>, A2aGatewayError>,
    > + Send;

    /// Forward a `agent/getAuthenticatedExtendedCard` request.
    fn get_extended_agent_card(
        &self,
    ) -> impl Future<Output = Result<AgentCard, A2aGatewayError>> + Send;

    /// Forward a push notification config set request.
    fn set_push_config(
        &self,
        config: TaskPushNotificationConfig,
    ) -> impl Future<Output = Result<TaskPushNotificationConfig, A2aGatewayError>> + Send;

    /// Forward a push notification config get request.
    fn get_push_config(
        &self,
        task_id: &str,
        config_id: Option<&str>,
    ) -> impl Future<Output = Result<TaskPushNotificationConfig, A2aGatewayError>> + Send;

    /// Forward a push notification config list request.
    fn list_push_configs(
        &self,
        task_id: &str,
    ) -> impl Future<Output = Result<Vec<TaskPushNotificationConfig>, A2aGatewayError>> + Send;

    /// Forward a push notification config delete request.
    fn delete_push_config(
        &self,
        task_id: &str,
        config_id: &str,
    ) -> impl Future<Output = Result<(), A2aGatewayError>> + Send;
}

/// Trait for a named collection of A2A agents.
///
/// Provides agent lookup by name and card retrieval for the API layer.
/// The API layer is generic over this trait — concrete registries live
/// in `bitrouter-providers`.
pub trait A2aGateway: Send + Sync {
    /// The per-agent proxy type.
    type Agent: A2aProxy;

    /// Look up an agent by name, returning an error if not found.
    fn require_agent(&self, name: &str) -> Result<&Self::Agent, A2aGatewayError>;

    /// Return the agent card for the given name, with URL rewritten to the
    /// gateway's external address.
    fn get_card(&self, name: &str) -> impl Future<Output = Option<AgentCard>> + Send;
}
