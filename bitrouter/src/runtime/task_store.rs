//! In-memory A2A task store.

use std::collections::HashMap;
use std::sync::RwLock;

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::server::{StoredTask, TaskStore};
use bitrouter_a2a::task::Task;

/// In-memory implementation of [`TaskStore`] using a `RwLock<HashMap>`.
pub struct InMemoryTaskStore {
    tasks: RwLock<HashMap<String, StoredTask>>,
}

impl InMemoryTaskStore {
    pub fn new() -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
        }
    }
}

impl TaskStore for InMemoryTaskStore {
    fn create(&self, task: &Task) -> Result<u64, A2aError> {
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        if tasks.contains_key(&task.id) {
            return Err(A2aError::AlreadyExists {
                name: task.id.clone(),
            });
        }

        let version = 1;
        tasks.insert(
            task.id.clone(),
            StoredTask {
                task: task.clone(),
                version,
            },
        );
        Ok(version)
    }

    fn update(&self, task: &Task, prev_version: u64) -> Result<u64, A2aError> {
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        let stored = tasks.get(&task.id).ok_or_else(|| A2aError::TaskNotFound {
            id: task.id.clone(),
        })?;

        if stored.version != prev_version {
            return Err(A2aError::VersionConflict);
        }

        let new_version = prev_version + 1;
        tasks.insert(
            task.id.clone(),
            StoredTask {
                task: task.clone(),
                version: new_version,
            },
        );
        Ok(new_version)
    }

    fn get(&self, task_id: &str) -> Result<Option<StoredTask>, A2aError> {
        let tasks = self
            .tasks
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        Ok(tasks.get(task_id).cloned())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_a2a::task::{TaskState, TaskStatus};

    fn make_task(id: &str, state: TaskState) -> Task {
        Task {
            id: id.to_string(),
            context_id: None,
            status: TaskStatus {
                state,
                timestamp: "2026-03-19T00:00:00Z".to_string(),
                message: None,
            },
            artifacts: Vec::new(),
            history: Vec::new(),
        }
    }

    #[test]
    fn create_and_get() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t-1", TaskState::Submitted);

        let version = store.create(&task).expect("create should succeed");
        assert_eq!(version, 1);

        let stored = store.get("t-1").expect("get should succeed");
        assert!(stored.is_some());
        let stored = stored.expect("should be Some");
        assert_eq!(stored.task.id, "t-1");
        assert_eq!(stored.version, 1);
    }

    #[test]
    fn create_duplicate_fails() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t-1", TaskState::Submitted);

        store.create(&task).expect("first create");
        let err = store.create(&task).expect_err("second create should fail");
        assert!(matches!(err, A2aError::AlreadyExists { .. }));
    }

    #[test]
    fn update_with_correct_version() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t-1", TaskState::Submitted);
        let v1 = store.create(&task).expect("create");

        let updated = make_task("t-1", TaskState::Completed);
        let v2 = store.update(&updated, v1).expect("update");
        assert_eq!(v2, 2);

        let stored = store.get("t-1").expect("get").expect("should exist");
        assert_eq!(stored.task.status.state, TaskState::Completed);
    }

    #[test]
    fn update_with_wrong_version_fails() {
        let store = InMemoryTaskStore::new();
        let task = make_task("t-1", TaskState::Submitted);
        store.create(&task).expect("create");

        let updated = make_task("t-1", TaskState::Completed);
        let err = store
            .update(&updated, 999)
            .expect_err("wrong version should fail");
        assert!(matches!(err, A2aError::VersionConflict));
    }

    #[test]
    fn get_nonexistent_returns_none() {
        let store = InMemoryTaskStore::new();
        let result = store.get("does-not-exist").expect("get should not error");
        assert!(result.is_none());
    }
}
