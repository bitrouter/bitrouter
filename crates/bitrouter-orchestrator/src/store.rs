//! Transactional facts used by execution and, later, checkpoint recovery.
//! A successful commit must be durable for the backend's documented fault scope.

use std::collections::HashMap;
use std::sync::Mutex;

use bitrouter_sdk::language_model::{Message, Prompt, Usage};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::agent::AgentConfig;
use crate::service::TaskEvent;

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CallRecord {
    pub item_id: String,
    pub provider_call_id: String,
    pub name: String,
    pub arguments: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum ExecutionRecord {
    Accepted {
        owner_key_id: String,
        owner_user_id: String,
        fingerprint: String,
        config: Box<AgentConfig>,
        verification_command: Option<String>,
        event: TaskEvent,
    },
    Event {
        event: TaskEvent,
    },
    ModelRequest {
        step_id: String,
        context_version: u64,
        prompt: Box<Prompt>,
    },
    ModelResponse {
        step_id: String,
        item_id: String,
        request_id: String,
        requested_model: String,
        usage: Option<Usage>,
        estimated_spend_microusd: u64,
        message: Message,
        calls: Vec<CallRecord>,
    },
    ToolIntent {
        step_id: String,
        call: CallRecord,
    },
    ToolResult {
        step_id: String,
        item_id: String,
        message: Message,
        effect: EffectStatus,
    },
    Settled {
        messages: Vec<Message>,
        model_steps: u32,
        tool_calls: u32,
        estimated_spend_microusd: u64,
        active_duration_ms: u64,
    },
}

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum EffectStatus {
    Completed,
    NotExecuted,
    Unknown,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredExecution {
    pub execution_id: String,
    pub version: u64,
    pub records: Vec<ExecutionRecord>,
}

#[async_trait::async_trait]
pub trait ExecutionStore: Send + Sync {
    /// Atomically append and compare the durable version. Version zero creates
    /// a new execution; a conflicting version must never overwrite records.
    async fn commit(
        &self,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String>;

    async fn load(&self, execution_id: &str) -> Result<Option<StoredExecution>, String>;
}

/// Explicitly volatile store for embedded agents and deterministic tests.
/// The shipped host supplies its database-backed implementation instead.
#[derive(Default)]
pub struct MemoryExecutionStore {
    records: Mutex<HashMap<String, StoredExecution>>,
}

#[async_trait::async_trait]
impl ExecutionStore for MemoryExecutionStore {
    async fn commit(
        &self,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        let mut entries = self
            .records
            .lock()
            .map_err(|_| "execution store lock poisoned".to_string())?;
        let actual = entries.get(execution_id).map_or(0, |entry| entry.version);
        if actual != expected_version || records.is_empty() {
            return Err("execution commit version conflict or empty batch".into());
        }
        let version = actual
            .checked_add(u64::try_from(records.len()).map_err(|error| error.to_string())?)
            .ok_or("execution version exhausted")?;
        let entry = entries
            .entry(execution_id.to_string())
            .or_insert_with(|| StoredExecution {
                execution_id: execution_id.into(),
                version: 0,
                records: Vec::new(),
            });
        entry.records.extend_from_slice(records);
        entry.version = version;
        Ok(version)
    }

    async fn load(&self, execution_id: &str) -> Result<Option<StoredExecution>, String> {
        Ok(self
            .records
            .lock()
            .map_err(|_| "execution store lock poisoned".to_string())?
            .get(execution_id)
            .cloned())
    }
}

pub(crate) struct CommitRequest {
    pub(crate) records: Vec<ExecutionRecord>,
    pub(crate) response: oneshot::Sender<Result<(), String>>,
}
