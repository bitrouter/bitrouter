//! A2A server-side traits and types.
//!
//! Provides the core abstractions for hosting an A2A server:
//! - [`TaskStore`] for persisting task state
//! - [`AgentExecutor`] for converting incoming A2A messages into agent work

use std::future::Future;

use crate::error::A2aError;
use crate::message::Message;
use crate::task::Task;

/// Versioned wrapper for stored tasks (optimistic concurrency control).
#[derive(Debug, Clone)]
pub struct StoredTask {
    /// The stored task.
    pub task: Task,
    /// Monotonically increasing version for OCC.
    pub version: u64,
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
}
