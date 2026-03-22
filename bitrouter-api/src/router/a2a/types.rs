//! Type re-exports and combined trait for A2A gateway filters.

pub(crate) use bitrouter_a2a::error::A2aGatewayError;
pub(crate) use bitrouter_a2a::server::{A2aDiscovery, A2aProxy};
pub(crate) use bitrouter_a2a::types::{
    CancelTaskRequest, DeleteTaskPushNotificationConfigRequest,
    GetTaskPushNotificationConfigRequest, GetTaskRequest, JsonRpcRequest, JsonRpcResponse,
    ListTaskPushNotificationConfigsRequest, ListTasksRequest, SendMessageRequest, StreamResponse,
    SubscribeToTaskRequest, TaskPushNotificationConfig,
};

/// Combined trait for an A2A gateway server.
pub trait A2aGateway: A2aDiscovery + A2aProxy {}
impl<T: A2aDiscovery + A2aProxy> A2aGateway for T {}
