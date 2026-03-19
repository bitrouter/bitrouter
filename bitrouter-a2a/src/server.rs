//! A2A server-side traits and types.
//!
//! Provides the core abstractions for hosting an A2A server:
//! - [`TaskStore`] for persisting task state
//! - [`AgentExecutor`] for converting incoming A2A messages into agent work

use std::future::Future;
use std::pin::Pin;

use futures_core::Stream;

use crate::error::A2aError;
use crate::message::Message;
use crate::request::{SendMessageConfiguration, TaskPushNotificationConfig};
use crate::stream::StreamResponse;
use crate::task::{Task, TaskState};

/// Versioned wrapper for stored tasks (optimistic concurrency control).
#[derive(Debug, Clone)]
pub struct StoredTask {
    /// The stored task.
    pub task: Task,
    /// Monotonically increasing version for OCC.
    pub version: u64,
}

/// Query parameters for listing tasks.
#[derive(Debug, Clone, Default)]
pub struct TaskQuery {
    /// Filter by context ID.
    pub context_id: Option<String>,
    /// Filter by task state.
    pub status: Option<TaskState>,
    /// Filter tasks with status timestamp after this ISO 8601 value.
    pub status_timestamp_after: Option<String>,
    /// Maximum number of tasks per page.
    pub page_size: Option<u32>,
    /// Cursor for pagination (task ID to start after).
    pub page_token: Option<String>,
}

/// Persistent storage for A2A task state.
pub trait TaskStore: Send + Sync {
    /// Create a new task. Returns the initial version.
    fn create(&self, task: &Task) -> Result<u64, A2aError>;

    /// Update an existing task. Returns the new version on success.
    /// Fails with `VersionConflict` if `prev_version` does not match.
    fn update(&self, task: &Task, prev_version: u64) -> Result<u64, A2aError>;

    /// Retrieve a task by ID. Returns `None` if not found.
    fn get(&self, task_id: &str) -> Result<Option<StoredTask>, A2aError>;

    /// List tasks matching a query. Returns matching tasks and an optional
    /// next-page cursor.
    fn list(&self, query: &TaskQuery) -> Result<(Vec<StoredTask>, Option<String>), A2aError>;
}

/// Context passed to the executor when handling a request.
#[derive(Debug, Clone)]
pub struct ExecutorContext {
    /// The incoming user message that triggered execution.
    pub message: Message,
    /// The task ID for this execution.
    pub task_id: String,
    /// Logical conversation grouping.
    pub context_id: String,
    /// Optional client configuration for the request.
    pub configuration: Option<SendMessageConfiguration>,
}

/// The result of executing an A2A request.
#[derive(Debug, Clone)]
pub enum ExecuteResult {
    /// Return a completed task with status and optional artifacts.
    Task(Task),
    /// Return a direct message response (no task lifecycle).
    Message(Message),
}

/// Converts incoming A2A messages into agent work (e.g., LLM calls)
/// and returns A2A-typed results.
pub trait AgentExecutor: Send + Sync {
    /// Execute a task given the incoming message and context.
    fn execute(
        &self,
        ctx: &ExecutorContext,
    ) -> impl Future<Output = Result<ExecuteResult, A2aError>> + Send;

    /// Cancel a running task.
    fn cancel(&self, task_id: &str) -> impl Future<Output = Result<Task, A2aError>> + Send;

    /// Execute a task and return a stream of responses.
    ///
    /// Default implementation calls [`execute`](Self::execute) and wraps the
    /// result in a single-item stream.
    fn execute_streaming(
        &self,
        ctx: &ExecutorContext,
    ) -> impl Future<
        Output = Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send + Sync>>, A2aError>,
    > + Send {
        let ctx = ctx.clone();
        async move {
            let result = self.execute(&ctx).await?;
            let event = match result {
                ExecuteResult::Task(task) => StreamResponse::Task(task),
                ExecuteResult::Message(msg) => StreamResponse::Message(msg),
            };
            let stream: Pin<Box<dyn Stream<Item = StreamResponse> + Send + Sync>> =
                Box::pin(OnceStream(Some(event)));
            Ok(stream)
        }
    }

    /// Subscribe to updates for an existing task.
    ///
    /// Default implementation returns an error indicating streaming is not
    /// supported.
    fn subscribe(
        &self,
        _task_id: &str,
    ) -> impl Future<
        Output = Result<Pin<Box<dyn Stream<Item = StreamResponse> + Send + Sync>>, A2aError>,
    > + Send {
        async { Err(A2aError::StreamingNotSupported) }
    }
}

/// A stream that yields a single item and then completes.
struct OnceStream<T>(Option<T>);

impl<T: Unpin + Send + Sync> Stream for OnceStream<T> {
    type Item = T;

    fn poll_next(
        mut self: Pin<&mut Self>,
        _cx: &mut std::task::Context<'_>,
    ) -> std::task::Poll<Option<T>> {
        std::task::Poll::Ready(self.0.take())
    }
}

/// Persistent storage for push notification configurations.
pub trait PushNotificationStore: Send + Sync {
    /// Create or update a push notification config. Returns the stored config
    /// (with generated ID if not provided).
    fn create_config(
        &self,
        config: &TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aError>;

    /// Get a specific push notification config.
    fn get_config(
        &self,
        task_id: &str,
        id: &str,
    ) -> Result<Option<TaskPushNotificationConfig>, A2aError>;

    /// List all push notification configs for a task.
    fn list_configs(&self, task_id: &str) -> Result<Vec<TaskPushNotificationConfig>, A2aError>;

    /// Delete a push notification config.
    fn delete_config(&self, task_id: &str, id: &str) -> Result<(), A2aError>;
}
