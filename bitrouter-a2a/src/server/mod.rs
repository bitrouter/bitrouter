//! A2A gateway server traits.
//!
//! Provides the [`A2aDiscovery`] and [`A2aProxy`] traits for serving
//! downstream A2A clients by proxying to an upstream agent.

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;
use tokio::sync::broadcast;

use crate::card::AgentCard;
use crate::error::A2aGatewayError;
use crate::request::{SendMessageRequest, TaskPushNotificationConfig};
use crate::stream::StreamResponse;
use crate::task::{GetTaskRequest, ListTasksRequest, ListTasksResponse, Task};

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
        request: crate::request::CancelTaskRequest,
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
