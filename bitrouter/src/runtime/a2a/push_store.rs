//! In-memory push notification configuration store.

use std::collections::HashMap;
use std::sync::RwLock;
use std::sync::atomic::{AtomicU64, Ordering};

use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::request::TaskPushNotificationConfig;
use bitrouter_a2a::server::PushNotificationStore;

/// In-memory implementation of [`PushNotificationStore`].
pub struct InMemoryPushNotificationStore {
    /// Keyed by `(task_id, config_id)`.
    configs: RwLock<HashMap<(String, String), TaskPushNotificationConfig>>,
}

impl InMemoryPushNotificationStore {
    pub fn new() -> Self {
        Self {
            configs: RwLock::new(HashMap::new()),
        }
    }
}

fn generate_config_id() -> String {
    static COUNTER: AtomicU64 = AtomicU64::new(1);
    format!("pnc-{}", COUNTER.fetch_add(1, Ordering::Relaxed))
}

impl PushNotificationStore for InMemoryPushNotificationStore {
    fn create_config(
        &self,
        config: &TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aError> {
        let task_id = config
            .task_id
            .clone()
            .ok_or_else(|| A2aError::Client("task_id is required".to_string()))?;

        let config_id = config.id.clone().unwrap_or_else(generate_config_id);

        let stored = TaskPushNotificationConfig {
            id: Some(config_id.clone()),
            task_id: Some(task_id.clone()),
            ..config.clone()
        };

        let mut configs = self
            .configs
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        configs.insert((task_id, config_id), stored.clone());
        Ok(stored)
    }

    fn get_config(
        &self,
        task_id: &str,
        id: &str,
    ) -> Result<Option<TaskPushNotificationConfig>, A2aError> {
        let configs = self
            .configs
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        Ok(configs.get(&(task_id.to_string(), id.to_string())).cloned())
    }

    fn list_configs(&self, task_id: &str) -> Result<Vec<TaskPushNotificationConfig>, A2aError> {
        let configs = self
            .configs
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        let result: Vec<_> = configs
            .iter()
            .filter(|((tid, _), _)| tid == task_id)
            .map(|(_, v)| v.clone())
            .collect();

        Ok(result)
    }

    fn delete_config(&self, task_id: &str, id: &str) -> Result<(), A2aError> {
        let mut configs = self
            .configs
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;

        let key = (task_id.to_string(), id.to_string());
        if configs.remove(&key).is_none() {
            return Err(A2aError::PushNotificationNotFound {
                task_id: task_id.to_string(),
                id: id.to_string(),
            });
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_config(task_id: &str) -> TaskPushNotificationConfig {
        TaskPushNotificationConfig {
            tenant: None,
            id: None,
            task_id: Some(task_id.to_string()),
            url: "https://example.com/webhook".to_string(),
            token: None,
            authentication: None,
        }
    }

    #[test]
    fn create_and_get() {
        let store = InMemoryPushNotificationStore::new();
        let config = make_config("task-1");
        let stored = store.create_config(&config).expect("create");

        assert!(stored.id.is_some());
        assert_eq!(stored.task_id.as_deref(), Some("task-1"));

        let fetched = store
            .get_config("task-1", stored.id.as_deref().expect("has id"))
            .expect("get");
        assert!(fetched.is_some());
    }

    #[test]
    fn list_configs_by_task() {
        let store = InMemoryPushNotificationStore::new();
        store.create_config(&make_config("task-1")).expect("create");
        store.create_config(&make_config("task-1")).expect("create");
        store.create_config(&make_config("task-2")).expect("create");

        let list = store.list_configs("task-1").expect("list");
        assert_eq!(list.len(), 2);

        let list = store.list_configs("task-2").expect("list");
        assert_eq!(list.len(), 1);
    }

    #[test]
    fn delete_config() {
        let store = InMemoryPushNotificationStore::new();
        let stored = store.create_config(&make_config("task-1")).expect("create");
        let id = stored.id.as_deref().expect("has id");

        store.delete_config("task-1", id).expect("delete");
        let fetched = store.get_config("task-1", id).expect("get");
        assert!(fetched.is_none());
    }

    #[test]
    fn delete_nonexistent_fails() {
        let store = InMemoryPushNotificationStore::new();
        let err = store
            .delete_config("task-1", "nope")
            .expect_err("should fail");
        assert!(matches!(err, A2aError::PushNotificationNotFound { .. }));
    }
}
