//! Shared utilities for provider implementations.

/// Guard that aborts background refresh tasks on drop.
///
/// Used by both MCP and A2A registries to manage background listeners
/// that watch for upstream capability changes.
pub struct RefreshGuard {
    handles: Vec<tokio::task::JoinHandle<()>>,
}

impl RefreshGuard {
    /// Build a guard from a list of already-spawned task handles.
    pub fn from_handles(handles: Vec<tokio::task::JoinHandle<()>>) -> Self {
        Self { handles }
    }
}

impl Drop for RefreshGuard {
    fn drop(&mut self) {
        for handle in &self.handles {
            handle.abort();
        }
    }
}
