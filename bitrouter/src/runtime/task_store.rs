//! In-memory A2A task store.

use std::collections::HashMap;
use std::sync::RwLock;

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::server::{StoredTask, TaskQuery, TaskStore};
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

/// Default page size for `list` queries.
const DEFAULT_PAGE_SIZE: u32 = 50;
/// Maximum page size for `list` queries.
const MAX_PAGE_SIZE: u32 = 100;

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

    fn list(&self, query: &TaskQuery) -> Result<(Vec<StoredTask>, Option<String>), A2aError> {
        let tasks = self
            .tasks
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        // Collect and filter.
        let mut matching: Vec<&StoredTask> = tasks
            .values()
            .filter(|stored| {
                if let Some(ref ctx) = query.context_id
                    && stored.task.context_id.as_deref() != Some(ctx)
                {
                    return false;
                }
                if let Some(ref status) = query.status
                    && &stored.task.status.state != status
                {
                    return false;
                }
                if let Some(ref after) = query.status_timestamp_after
                    && stored.task.status.timestamp.as_str() <= after.as_str()
                {
                    return false;
                }
                true
            })
            .collect();

        // Sort by task ID for deterministic pagination.
        matching.sort_by(|a, b| a.task.id.cmp(&b.task.id));

        // Apply cursor-based pagination.
        if let Some(ref token) = query.page_token {
            // Skip tasks until past the cursor.
            let pos = matching
                .iter()
                .position(|s| s.task.id.as_str() > token.as_str());
            if let Some(pos) = pos {
                matching = matching[pos..].to_vec();
            } else {
                matching.clear();
            }
        }

        let page_size = query
            .page_size
            .unwrap_or(DEFAULT_PAGE_SIZE)
            .min(MAX_PAGE_SIZE) as usize;

        let next_page_token = if matching.len() > page_size {
            matching[page_size - 1].task.id.clone().into()
        } else {
            None
        };

        let page: Vec<StoredTask> = matching.into_iter().take(page_size).cloned().collect();

        Ok((page, next_page_token))
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
            metadata: None,
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

    #[test]
    fn list_empty_store() {
        let store = InMemoryTaskStore::new();
        let query = TaskQuery::default();
        let (tasks, next) = store.list(&query).expect("list");
        assert!(tasks.is_empty());
        assert!(next.is_none());
    }

    #[test]
    fn list_with_status_filter() {
        let store = InMemoryTaskStore::new();
        store
            .create(&make_task("t-1", TaskState::Submitted))
            .expect("create");
        store
            .create(&make_task("t-2", TaskState::Completed))
            .expect("create");
        store
            .create(&make_task("t-3", TaskState::Submitted))
            .expect("create");

        let query = TaskQuery {
            status: Some(TaskState::Submitted),
            ..Default::default()
        };
        let (tasks, _) = store.list(&query).expect("list");
        assert_eq!(tasks.len(), 2);
        assert!(
            tasks
                .iter()
                .all(|t| t.task.status.state == TaskState::Submitted)
        );
    }

    #[test]
    fn list_with_context_filter() {
        let store = InMemoryTaskStore::new();
        let mut task = make_task("t-1", TaskState::Submitted);
        task.context_id = Some("ctx-a".to_string());
        store.create(&task).expect("create");

        let mut task = make_task("t-2", TaskState::Submitted);
        task.context_id = Some("ctx-b".to_string());
        store.create(&task).expect("create");

        let query = TaskQuery {
            context_id: Some("ctx-a".to_string()),
            ..Default::default()
        };
        let (tasks, _) = store.list(&query).expect("list");
        assert_eq!(tasks.len(), 1);
        assert_eq!(tasks[0].task.id, "t-1");
    }

    #[test]
    fn list_pagination() {
        let store = InMemoryTaskStore::new();
        for i in 0..5 {
            store
                .create(&make_task(&format!("t-{i:02}"), TaskState::Submitted))
                .expect("create");
        }

        let query = TaskQuery {
            page_size: Some(2),
            ..Default::default()
        };
        let (page1, next) = store.list(&query).expect("list");
        assert_eq!(page1.len(), 2);
        assert!(next.is_some());

        let query2 = TaskQuery {
            page_size: Some(2),
            page_token: next,
            ..Default::default()
        };
        let (page2, _) = store.list(&query2).expect("list");
        assert_eq!(page2.len(), 2);
        // Pages should not overlap.
        assert_ne!(page1[0].task.id, page2[0].task.id);
    }
}
