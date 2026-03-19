//! Shared mock implementations and test helpers for A2A router tests.

use std::collections::HashMap;
use std::sync::{Arc, RwLock};

use bitrouter_a2a::card::minimal_card;
use bitrouter_a2a::error::A2aError;
use bitrouter_a2a::message::{Message, MessageRole, Part};
use bitrouter_a2a::registry::{AgentCardRegistry, AgentRegistration};
use bitrouter_a2a::request::TaskPushNotificationConfig;
use bitrouter_a2a::server::{
    AgentExecutor, ExecuteResult, ExecutorContext, PushNotificationStore, StoredTask, TaskQuery,
    TaskStore,
};
use bitrouter_a2a::task::{Task, TaskState, TaskStatus};

use warp::Filter;

// ── Mock implementations ───────────────────────────────────

pub struct MockExecutor;

impl AgentExecutor for MockExecutor {
    async fn execute(&self, ctx: &ExecutorContext) -> Result<ExecuteResult, A2aError> {
        // Echo back the input text as the agent response.
        let input_text = ctx
            .message
            .parts
            .iter()
            .filter_map(|p| p.text.as_deref())
            .collect::<Vec<_>>()
            .join(" ");

        let response_msg = Message {
            role: MessageRole::Agent,
            parts: vec![Part::text(&format!("Echo: {input_text}"))],
            message_id: format!("{}-resp", ctx.task_id),
            context_id: Some(ctx.context_id.clone()),
            task_id: Some(ctx.task_id.clone()),
            reference_task_ids: Vec::new(),
            metadata: None,
            extensions: Vec::new(),
        };

        let task = Task {
            id: ctx.task_id.clone(),
            context_id: Some(ctx.context_id.clone()),
            status: TaskStatus {
                state: TaskState::Completed,
                timestamp: "2026-03-19T00:00:00Z".to_string(),
                message: Some(response_msg),
            },
            artifacts: Vec::new(),
            history: Vec::new(),
            metadata: None,
        };
        Ok(ExecuteResult::Task(task))
    }

    async fn cancel(&self, task_id: &str) -> Result<Task, A2aError> {
        Ok(Task {
            id: task_id.to_string(),
            context_id: None,
            status: TaskStatus {
                state: TaskState::Canceled,
                timestamp: "2026-03-19T00:00:00Z".to_string(),
                message: None,
            },
            artifacts: Vec::new(),
            history: Vec::new(),
            metadata: None,
        })
    }
}

pub struct MockTaskStore {
    tasks: RwLock<HashMap<String, StoredTask>>,
}

impl MockTaskStore {
    pub fn new() -> Self {
        Self {
            tasks: RwLock::new(HashMap::new()),
        }
    }
}

impl TaskStore for MockTaskStore {
    fn create(&self, task: &Task) -> Result<u64, A2aError> {
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        tasks.insert(
            task.id.clone(),
            StoredTask {
                task: task.clone(),
                version: 1,
            },
        );
        Ok(1)
    }

    fn update(&self, task: &Task, _prev_version: u64) -> Result<u64, A2aError> {
        let mut tasks = self
            .tasks
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        let version = tasks.get(&task.id).map_or(1, |s| s.version + 1);
        tasks.insert(
            task.id.clone(),
            StoredTask {
                task: task.clone(),
                version,
            },
        );
        Ok(version)
    }

    fn get(&self, task_id: &str) -> Result<Option<StoredTask>, A2aError> {
        let tasks = self
            .tasks
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        Ok(tasks.get(task_id).cloned())
    }

    fn list(&self, _query: &TaskQuery) -> Result<(Vec<StoredTask>, Option<String>), A2aError> {
        let tasks = self
            .tasks
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        let all: Vec<StoredTask> = tasks.values().cloned().collect();
        Ok((all, None))
    }
}

pub struct MockPushStore;

impl PushNotificationStore for MockPushStore {
    fn create_config(
        &self,
        config: &TaskPushNotificationConfig,
    ) -> Result<TaskPushNotificationConfig, A2aError> {
        let mut result = config.clone();
        if result.id.is_none() {
            result.id = Some("cfg-1".to_string());
        }
        Ok(result)
    }

    fn get_config(
        &self,
        _task_id: &str,
        _id: &str,
    ) -> Result<Option<TaskPushNotificationConfig>, A2aError> {
        Ok(None)
    }

    fn list_configs(&self, _task_id: &str) -> Result<Vec<TaskPushNotificationConfig>, A2aError> {
        Ok(Vec::new())
    }

    fn delete_config(&self, _task_id: &str, _id: &str) -> Result<(), A2aError> {
        Ok(())
    }
}

/// In-memory mock registry for tests (replaces FileAgentCardRegistry dependency).
pub struct MockRegistry {
    entries: RwLock<HashMap<String, AgentRegistration>>,
}

impl MockRegistry {
    pub fn new() -> Self {
        Self {
            entries: RwLock::new(HashMap::new()),
        }
    }
}

impl AgentCardRegistry for MockRegistry {
    fn register(&self, registration: AgentRegistration) -> Result<(), A2aError> {
        let mut entries = self
            .entries
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        let name = registration.card.name.clone();
        if entries.contains_key(&name) {
            return Err(A2aError::AlreadyExists { name });
        }
        entries.insert(name, registration);
        Ok(())
    }

    fn remove(&self, name: &str) -> Result<(), A2aError> {
        let mut entries = self
            .entries
            .write()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        if entries.remove(name).is_none() {
            return Err(A2aError::NotFound {
                name: name.to_string(),
            });
        }
        Ok(())
    }

    fn get(&self, name: &str) -> Result<Option<AgentRegistration>, A2aError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        Ok(entries.get(name).cloned())
    }

    fn list(&self) -> Result<Vec<AgentRegistration>, A2aError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        let mut list: Vec<_> = entries.values().cloned().collect();
        list.sort_by(|a, b| a.card.name.cmp(&b.card.name));
        Ok(list)
    }

    fn resolve_by_iss(&self, iss: &str) -> Result<Option<String>, A2aError> {
        let entries = self
            .entries
            .read()
            .map_err(|e| A2aError::Storage(format!("lock poisoned: {e}")))?;
        for reg in entries.values() {
            if reg.iss.as_deref() == Some(iss) {
                return Ok(Some(reg.card.name.clone()));
            }
        }
        Ok(None)
    }
}

// ── Helper functions ────────────────────────────────────────

/// Creates an empty `MockRegistry` wrapped in `Arc`.
pub fn setup_empty_registry() -> Arc<MockRegistry> {
    Arc::new(MockRegistry::new())
}

/// Creates a `MockRegistry` with a pre-registered "test-agent".
pub fn setup_registry() -> Arc<MockRegistry> {
    let registry = Arc::new(MockRegistry::new());
    registry
        .register(AgentRegistration {
            card: minimal_card(
                "test-agent",
                "A test agent",
                "1.0.0",
                "http://localhost/a2a",
            ),
            iss: None,
        })
        .expect("register");
    registry
}

/// Builds a full JSON-RPC filter with mocks and a pre-registered agent.
pub fn build_jsonrpc_filter() -> (
    impl Filter<Extract = (impl warp::Reply,), Error = warp::Rejection> + Clone,
    Arc<MockRegistry>,
) {
    let registry = setup_registry();

    let filter = super::jsonrpc::jsonrpc_filter(
        Arc::new(MockExecutor),
        Arc::new(MockTaskStore::new()),
        registry.clone(),
        Arc::new(MockPushStore),
    );
    (filter, registry)
}

pub fn jsonrpc_body(method: &str, params: serde_json::Value) -> String {
    serde_json::json!({
        "jsonrpc": "2.0",
        "id": "test-1",
        "method": method,
        "params": params
    })
    .to_string()
}
