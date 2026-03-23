//! A2A transport trait and implementations.
//!
//! Defines [`A2aTransport`] — the wire-level interface for communicating
//! with upstream A2A agents. Implementations handle protocol framing
//! (JSON-RPC, REST, gRPC) and wire format conversion.

use std::pin::Pin;

use futures_core::Stream;

use crate::error::A2aGatewayError;
use crate::types::{
    AgentCard, CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, GetTaskRequest, ListTaskPushNotificationConfigsRequest,
    ListTasksRequest, ListTasksResponse, SendMessageRequest, SendMessageResult, StreamResponse,
    Task, TaskPushNotificationConfig,
};

/// Boxed streaming result returned by transport streaming methods.
type StreamingResult = Result<
    Pin<Box<dyn Stream<Item = Result<StreamResponse, A2aGatewayError>> + Send>>,
    A2aGatewayError,
>;

/// Wire-level transport for communicating with an upstream A2A agent.
///
/// Implementors handle protocol framing (JSON-RPC 2.0, HTTP+JSON REST, gRPC)
/// and wire format conversion. Higher-level concerns like card caching and
/// URL rewriting are managed by [`UpstreamA2aAgent`](crate::client::upstream::UpstreamA2aAgent).
pub trait A2aTransport: Send + Sync {
    /// Fetch an Agent Card from a remote server's well-known endpoint.
    fn discover(
        &self,
        base_url: &str,
    ) -> impl Future<Output = Result<AgentCard, A2aGatewayError>> + Send;

    /// Fetch an extended Agent Card via authenticated endpoint.
    fn get_extended_agent_card(
        &self,
        endpoint: &str,
    ) -> impl Future<Output = Result<AgentCard, A2aGatewayError>> + Send;

    /// Send a message to a remote agent.
    fn send_message(
        &self,
        endpoint: &str,
        request: SendMessageRequest,
    ) -> impl Future<Output = Result<SendMessageResult, A2aGatewayError>> + Send;

    /// Get the current state of a task.
    fn get_task(
        &self,
        endpoint: &str,
        request: GetTaskRequest,
    ) -> impl Future<Output = Result<Task, A2aGatewayError>> + Send;

    /// Cancel a running task.
    fn cancel_task(
        &self,
        endpoint: &str,
        request: CancelTaskRequest,
    ) -> impl Future<Output = Result<Task, A2aGatewayError>> + Send;

    /// List tasks matching a query.
    fn list_tasks(
        &self,
        endpoint: &str,
        request: ListTasksRequest,
    ) -> impl Future<Output = Result<ListTasksResponse, A2aGatewayError>> + Send;

    /// Send a streaming message to a remote agent.
    fn send_streaming_message(
        &self,
        _endpoint: &str,
        _request: SendMessageRequest,
    ) -> impl Future<Output = StreamingResult> + Send {
        std::future::ready(Err(A2aGatewayError::Client(
            "streaming not supported by this transport".to_string(),
        )))
    }

    /// Subscribe to task updates.
    fn subscribe_to_task(
        &self,
        _endpoint: &str,
        _task_id: &str,
    ) -> impl Future<Output = StreamingResult> + Send {
        std::future::ready(Err(A2aGatewayError::Client(
            "task subscription not supported by this transport".to_string(),
        )))
    }

    /// Set (create or update) a push notification configuration.
    fn set_push_config(
        &self,
        endpoint: &str,
        config: TaskPushNotificationConfig,
    ) -> impl Future<Output = Result<TaskPushNotificationConfig, A2aGatewayError>> + Send;

    /// Get a push notification configuration.
    fn get_push_config(
        &self,
        endpoint: &str,
        request: GetTaskPushNotificationConfigRequest,
    ) -> impl Future<Output = Result<TaskPushNotificationConfig, A2aGatewayError>> + Send;

    /// List push notification configurations for a task.
    fn list_push_configs(
        &self,
        endpoint: &str,
        request: ListTaskPushNotificationConfigsRequest,
    ) -> impl Future<Output = Result<Vec<TaskPushNotificationConfig>, A2aGatewayError>> + Send;

    /// Delete a push notification configuration.
    fn delete_push_config(
        &self,
        endpoint: &str,
        request: DeleteTaskPushNotificationConfigRequest,
    ) -> impl Future<Output = Result<(), A2aGatewayError>> + Send;
}

pub mod http;
pub mod jsonrpc;

// gRPC transport is not yet implemented.
// Enable with the `client-grpc` feature when available.
// #[cfg(feature = "client-grpc")]
// pub mod grpc;
