//! Transactional facts used by execution and, later, checkpoint recovery.
//! A successful commit must be durable for the backend's documented fault scope.

use std::collections::HashMap;
use std::sync::Mutex;

use bitrouter_sdk::language_model::{Message, Prompt, Usage};
use serde::{Deserialize, Serialize};
use tokio::sync::oneshot;

use crate::agent::AgentConfig;
use crate::item::CallRecord;
use crate::thread::{ThreadEvent, ThreadSnapshot};
use crate::turn::{SteeringReceipt, VerificationEvidence};

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AcceptedKey {
    pub scope: String,
    pub key: String,
    pub fingerprint: String,
    pub thread_id: String,
    pub turn_id: Option<String>,
}

/// A stopped owner is durable proof that its service sealed admission, joined
/// owned execution and confirmed cleanup before releasing write authority.
/// Abrupt process loss leaves this record active; a new epoch cannot retire it.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionOwner {
    pub server_instance_id: String,
    pub generation: u64,
    pub stopped_at_ms: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum OwnerClaim {
    Acquired { owner: ExecutionOwner },
    Blocked { owner: ExecutionOwner },
    Unfenced,
}

pub fn validate_owner_id(server_instance_id: &str) -> Result<(), String> {
    if server_instance_id.is_empty() || server_instance_id.len() > 128 {
        return Err("invalid execution owner identity".into());
    }
    Ok(())
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SettlementOutcome {
    pub status: crate::agent::RunStatus,
    pub final_answer: Option<String>,
    pub detail: String,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(tag = "record", rename_all = "snake_case")]
pub enum ExecutionRecord {
    HarnessInventory {
        context_version: u64,
        inventory: Box<crate::harness::HarnessInventory>,
    },
    /// Joined work and known effects/context are prepared for workspace release;
    /// the final outcome is committed before the held OS lock is dropped.
    WorkspaceReleasePrepared {
        workspace: std::path::PathBuf,
        execution_id: String,
        lease_id: String,
    },
    /// Bounded public projection, committed by the same owner as its facts.
    ThreadEvent {
        event: ThreadEvent,
    },
    ThreadCreated {
        caller: bitrouter_sdk::caller::CallerContext,
        snapshot: ThreadSnapshot,
        config: Box<AgentConfig>,
        verification_command: Option<String>,
    },
    AcceptedKey {
        entry: AcceptedKey,
    },
    TurnQueued {
        turn_id: String,
        user_item_id: String,
        prompt: String,
        queue_order: u64,
    },
    TurnActivated {
        turn_id: String,
        context_version: u64,
    },
    QueuedTurnCancelled {
        turn_id: String,
    },
    QueueResumed,
    ThreadRecovered {
        source_server_instance_id: String,
        source_cursor: u64,
    },
    SteeringReceived {
        receipt: SteeringReceipt,
        text: String,
        key: String,
    },
    SteeringResolved {
        receipt: SteeringReceipt,
    },
    ThreadCheckpoint {
        snapshot: ThreadSnapshot,
        messages: Vec<Message>,
    },
    /// Turn facts share the Thread transaction and commit owner.
    TurnRecord {
        turn_id: String,
        fact: Box<ExecutionRecord>,
    },
    TurnLifecycle {
        turn_id: String,
        lifecycle: crate::turn::TurnLifecycle,
    },
    ModelRequest {
        step_id: String,
        #[serde(default)]
        item_id: String,
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
    ModelInterrupted {
        step_id: String,
        item_id: String,
        request_id: Option<String>,
        usage: Option<Usage>,
        /// Presentation evidence only; never a complete context message.
        partial: Message,
        detail: String,
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
    VerificationResult {
        #[serde(default)]
        status: Option<crate::turn::VerificationStatus>,
        call: CallRecord,
        evidence: VerificationEvidence,
        effect: EffectStatus,
        active_duration_ms: u64,
        tool_calls: u32,
    },
    /// No request or effect is in flight. Continuing preserves the same Turn
    /// and consumes its cumulative budget before requesting another model step.
    RunCheckpoint {
        context_version: u64,
        messages: Vec<Message>,
        model_steps: u32,
        tool_calls: u32,
        estimated_spend_microusd: u64,
        active_duration_ms: u64,
    },
    Settled {
        #[serde(default)]
        outcome: Option<SettlementOutcome>,
        #[serde(default)]
        context_version: u64,
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

pub const RUNTIME_FORMAT_VERSION: u32 = 3;

pub fn validate_runtime_format(version: u32) -> Result<(), String> {
    if version != 2 && version != RUNTIME_FORMAT_VERSION {
        return Err(format!("unsupported_runtime_format: {version}"));
    }
    Ok(())
}

#[derive(Clone, Serialize, Deserialize)]
pub struct StoredExecution {
    #[serde(default)]
    pub format_version: u32,
    pub execution_id: String,
    pub version: u64,
    pub records: Vec<ExecutionRecord>,
}

#[async_trait::async_trait]
pub trait ExecutionStore: Send + Sync {
    /// Monotonic root positions at a fixed discovery cutoff. Heads describe
    /// current versions, not a snapshot of mutable journal content. Execution
    /// admission stays closed until a claimed owner finishes its startup scan.
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<ExecutionIndexPage, String>;

    /// Claim an empty store or take over a durably stopped owner. An active old
    /// owner or legacy unfenced facts must remain blocked, never time out.
    async fn claim_owner(&self, server_instance_id: &str) -> Result<OwnerClaim, String>;

    async fn read_owner(&self, server_instance_id: &str) -> Result<Option<ExecutionOwner>, String>;

    /// Only the current owner can commit its stopped proof. The caller must
    /// have sealed admission, joined workers and confirmed process cleanup.
    async fn stop_owner(&self, owner: &ExecutionOwner) -> Result<ExecutionOwner, String>;

    /// Owner fencing, version CAS, keys and facts share one transaction.
    async fn commit_owned(
        &self,
        owner: &ExecutionOwner,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String>;

    /// Atomically append and compare the durable version. Version zero creates
    /// a new execution; a conflicting version must never overwrite records.
    /// Bootstrap/import only: forbidden after any runtime owner is established.
    async fn commit(
        &self,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String>;

    async fn load(&self, execution_id: &str) -> Result<Option<StoredExecution>, String>;

    /// Bounded authoritative facts at a fixed cutoff. First-page None captures
    /// the current version; subsequent pages reuse it. No partial history is
    /// silently accepted as a complete recovery stream.
    async fn read_records(
        &self,
        execution_id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Option<ExecutionPage>, String>;

    /// Read public Thread transactions in (after, cutoff], in sequence order.
    /// `more` is true only when another event exists at this fixed cutoff.
    /// Backends must bound reads rather than load the entire execution stream.
    async fn thread_history(
        &self,
        execution_id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<ThreadHistoryChunk, String>;

    /// The key index and its acceptance records commit in the same transaction.
    async fn find_key(&self, scope: &str, key: &str) -> Result<Option<AcceptedKey>, String>;
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExecutionHead {
    pub format_version: u32,
    pub position: u64,
    pub execution_id: String,
    pub version: u64,
}

pub struct ExecutionIndexPage {
    pub cutoff: u64,
    pub entries: Vec<ExecutionHead>,
    pub next_after: Option<u64>,
}

pub struct ExecutionPage {
    pub cutoff: u64,
    pub records: Vec<ExecutionRecord>,
    pub next_after: Option<u64>,
}

pub struct ThreadHistoryChunk {
    pub events: Vec<ThreadEvent>,
    pub more: bool,
}

/// Shared byte admission for bounded history pages. Oversized single events
/// fail visibly; they are never silently skipped or partially reconstructed.
pub fn push_history_event(
    events: &mut Vec<ThreadEvent>,
    bytes: &mut usize,
    event: ThreadEvent,
    max_bytes: usize,
) -> Result<bool, String> {
    let size = serde_json::to_vec(&event)
        .map_err(|error| error.to_string())?
        .len();
    if bytes.saturating_add(size) > max_bytes {
        if events.is_empty() {
            return Err("Thread history event exceeds page byte bound".into());
        }
        return Ok(false);
    }
    *bytes = bytes.saturating_add(size);
    events.push(event);
    Ok(true)
}

/// Explicitly volatile store for embedded agents and deterministic tests.
/// The shipped host supplies its database-backed implementation instead.
#[derive(Default)]
pub struct MemoryExecutionStore {
    state: Mutex<MemoryState>,
}

#[derive(Default)]
struct MemoryState {
    index: std::collections::BTreeMap<u64, String>,
    records: HashMap<String, StoredExecution>,
    keys: HashMap<(String, String), AcceptedKey>,
    owner: Option<ExecutionOwner>,
    owners: HashMap<String, ExecutionOwner>,
}

#[cfg(test)]
impl MemoryExecutionStore {
    pub(crate) fn set_format_for_test(&self, id: &str, version: u32) -> Result<(), String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?;
        state
            .records
            .get_mut(id)
            .ok_or("unknown root")?
            .format_version = version;
        Ok(())
    }
}

#[async_trait::async_trait]
impl ExecutionStore for MemoryExecutionStore {
    async fn read_index(
        &self,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<ExecutionIndexPage, String> {
        if !(1..=128).contains(&limit) || !(1..=4 * 1024 * 1024).contains(&max_bytes) {
            return Err("invalid execution index bounds".into());
        }
        let state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?;
        let head = state
            .index
            .last_key_value()
            .map_or(0, |(position, _)| *position);
        let cutoff = cutoff.unwrap_or(head);
        if after > cutoff || cutoff > head {
            return Err("invalid execution index cutoff".into());
        }
        let mut entries = Vec::new();
        let mut bytes = 0_usize;
        let mut more = false;
        for (position, id) in state.index.range((
            std::ops::Bound::Excluded(after),
            std::ops::Bound::Included(cutoff),
        )) {
            let entry = state
                .records
                .get(id)
                .ok_or("execution index points to missing root")?;
            let value = ExecutionHead {
                format_version: entry.format_version,
                position: *position,
                execution_id: id.clone(),
                version: entry.version,
            };
            let size = serde_json::to_vec(&value).map_err(|e| e.to_string())?.len();
            if entries.len() == limit || bytes.saturating_add(size) > max_bytes {
                if entries.is_empty() {
                    return Err("execution index entry exceeds byte bound".into());
                }
                more = true;
                break;
            }
            bytes += size;
            entries.push(value);
        }
        let next_after = if more {
            entries.last().map(|entry| entry.position)
        } else {
            None
        };
        Ok(ExecutionIndexPage {
            cutoff,
            entries,
            next_after,
        })
    }

    async fn claim_owner(&self, server_instance_id: &str) -> Result<OwnerClaim, String> {
        validate_owner_id(server_instance_id)?;
        let mut state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?;
        if let Some(owner) = &state.owner {
            if owner.server_instance_id == server_instance_id && owner.stopped_at_ms.is_none() {
                return Ok(OwnerClaim::Acquired {
                    owner: owner.clone(),
                });
            }
            if owner.server_instance_id == server_instance_id || owner.stopped_at_ms.is_none() {
                return Ok(OwnerClaim::Blocked {
                    owner: owner.clone(),
                });
            }
        } else if !state.records.is_empty() {
            return Ok(OwnerClaim::Unfenced);
        }
        if state.owners.contains_key(server_instance_id) {
            return Err("retired execution owner cannot be reused".into());
        }
        let generation = state
            .owner
            .as_ref()
            .map_or(0, |owner| owner.generation)
            .checked_add(1)
            .ok_or("execution owner generation exhausted")?;
        let owner = ExecutionOwner {
            server_instance_id: server_instance_id.into(),
            generation,
            stopped_at_ms: None,
        };
        state
            .owners
            .insert(server_instance_id.into(), owner.clone());
        state.owner = Some(owner.clone());
        Ok(OwnerClaim::Acquired { owner })
    }

    async fn read_owner(&self, server_instance_id: &str) -> Result<Option<ExecutionOwner>, String> {
        validate_owner_id(server_instance_id)?;
        Ok(self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?
            .owners
            .get(server_instance_id)
            .cloned())
    }

    async fn stop_owner(&self, owner: &ExecutionOwner) -> Result<ExecutionOwner, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?;
        let stored = state
            .owner
            .as_mut()
            .ok_or("execution owner is not established")?;
        if stored.server_instance_id != owner.server_instance_id
            || stored.generation != owner.generation
        {
            return Err("execution owner fence changed".into());
        }
        if stored.stopped_at_ms.is_none() {
            stored.stopped_at_ms = Some(owner_time_ms()?);
        }
        let stopped = stored.clone();
        state
            .owners
            .insert(owner.server_instance_id.clone(), stopped.clone());
        Ok(stopped)
    }

    async fn commit_owned(
        &self,
        owner: &ExecutionOwner,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?;
        if state.owner.as_ref() != Some(owner) || owner.stopped_at_ms.is_some() {
            return Err("execution owner fence changed or stopped".into());
        }
        commit_memory(&mut state, execution_id, expected_version, records)
    }

    async fn read_records(
        &self,
        execution_id: &str,
        after: u64,
        cutoff: Option<u64>,
        limit: usize,
        max_bytes: usize,
    ) -> Result<Option<ExecutionPage>, String> {
        if limit == 0 || limit > 128 || max_bytes == 0 || max_bytes > 4 * 1024 * 1024 {
            return Err("invalid execution page bounds".into());
        }
        let state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?;
        let Some(stored) = state.records.get(execution_id) else {
            return Ok(None);
        };
        validate_runtime_format(stored.format_version)?;
        let cutoff = cutoff.unwrap_or(stored.version);
        if after > cutoff || cutoff > stored.version {
            return Err("invalid execution page cutoff".into());
        }
        let start = usize::try_from(after).map_err(|error| error.to_string())?;
        let end = usize::try_from(cutoff).map_err(|error| error.to_string())?;
        let slice = stored
            .records
            .get(start..end)
            .ok_or("incomplete execution stream")?;
        let mut records = Vec::new();
        let mut bytes: usize = 0;
        for record in slice.iter().take(limit) {
            let size = serde_json::to_vec(record)
                .map_err(|error| error.to_string())?
                .len();
            if bytes.saturating_add(size) > max_bytes {
                if records.is_empty() {
                    return Err("execution record exceeds page byte bound".into());
                }
                break;
            }
            bytes = bytes.saturating_add(size);
            records.push(record.clone());
        }
        let next = after
            .checked_add(u64::try_from(records.len()).map_err(|error| error.to_string())?)
            .ok_or("execution page cursor exhausted")?;
        Ok(Some(ExecutionPage {
            cutoff,
            records,
            next_after: (next < cutoff).then_some(next),
        }))
    }

    async fn thread_history(
        &self,
        execution_id: &str,
        after: u64,
        cutoff: u64,
        limit: usize,
        max_bytes: usize,
    ) -> Result<ThreadHistoryChunk, String> {
        if limit == 0 || limit > 1000 || max_bytes == 0 || after > cutoff {
            return Err("invalid Thread history bounds".into());
        }
        let state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?;
        let stored = state.records.get(execution_id).ok_or("unknown execution")?;
        validate_runtime_format(stored.format_version)?;
        if cutoff > stored.version {
            return Err("history cutoff is ahead of execution".into());
        }
        let mut events = Vec::new();
        let mut bytes = 0;
        let mut more = false;
        for (index, record) in stored
            .records
            .iter()
            .enumerate()
            .skip(usize::try_from(after).map_err(|error| error.to_string())?)
            .take(usize::try_from(cutoff - after).map_err(|error| error.to_string())?)
        {
            let seq = u64::try_from(index)
                .map_err(|error| error.to_string())?
                .checked_add(1)
                .ok_or("history cursor exhausted")?;
            let event = match record {
                ExecutionRecord::ThreadEvent { event } => {
                    if event.seq != seq || event.thread_id != execution_id {
                        return Err(
                            "Thread event cursor or identity does not match stored row".into()
                        );
                    }
                    Some(event.clone())
                }
                _ => None,
            };
            let Some(event) = event else {
                continue;
            };
            if events.len() == limit
                || !push_history_event(&mut events, &mut bytes, event, max_bytes)?
            {
                more = true;
                break;
            }
        }
        Ok(ThreadHistoryChunk { events, more })
    }

    async fn commit(
        &self,
        execution_id: &str,
        expected_version: u64,
        records: &[ExecutionRecord],
    ) -> Result<u64, String> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned".to_string())?;
        if state.owner.is_some() {
            return Err("unfenced commit forbidden after runtime ownership".into());
        }
        commit_memory(&mut state, execution_id, expected_version, records)
    }

    async fn load(&self, execution_id: &str) -> Result<Option<StoredExecution>, String> {
        let state = self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned")?;
        if let Some(root) = state.records.get(execution_id) {
            validate_runtime_format(root.format_version)?;
        }
        Ok(state.records.get(execution_id).cloned())
    }

    async fn find_key(&self, scope: &str, key: &str) -> Result<Option<AcceptedKey>, String> {
        Ok(self
            .state
            .lock()
            .map_err(|_| "execution store lock poisoned".to_string())?
            .keys
            .get(&(scope.into(), key.into()))
            .cloned())
    }
}

pub(crate) struct CommitRequest {
    pub(crate) records: Vec<ExecutionRecord>,
    pub(crate) response: oneshot::Sender<Result<(), String>>,
}

pub fn owner_time_ms() -> Result<u64, String> {
    let duration = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_err(|error| error.to_string())?;
    u64::try_from(duration.as_millis()).map_err(|error| error.to_string())
}

fn commit_memory(
    state: &mut MemoryState,
    execution_id: &str,
    expected_version: u64,
    records: &[ExecutionRecord],
) -> Result<u64, String> {
    if let Some(root) = state.records.get(execution_id) {
        validate_runtime_format(root.format_version)?;
    }
    let actual = state
        .records
        .get(execution_id)
        .map_or(0, |entry| entry.version);
    if actual != expected_version || records.is_empty() {
        return Err("execution commit version conflict or empty batch".into());
    }
    let keys = records
        .iter()
        .filter_map(|record| match record {
            ExecutionRecord::AcceptedKey { entry } => Some(entry),
            _ => None,
        })
        .collect::<Vec<_>>();
    let mut batch_keys = std::collections::HashSet::new();
    for key in &keys {
        let identity = (key.scope.clone(), key.key.clone());
        if key.thread_id != execution_id
            || state.keys.contains_key(&identity)
            || !batch_keys.insert(identity)
        {
            return Err("acceptance key conflict or mismatched Thread".into());
        }
    }
    let version = actual
        .checked_add(u64::try_from(records.len()).map_err(|error| error.to_string())?)
        .ok_or("execution version exhausted")?;
    if actual == 0 {
        let position = state
            .index
            .last_key_value()
            .map_or(0, |(position, _)| *position)
            .checked_add(1)
            .ok_or("execution discovery position exhausted")?;
        state.index.insert(position, execution_id.to_string());
    }
    let entry = state
        .records
        .entry(execution_id.to_string())
        .or_insert_with(|| StoredExecution {
            format_version: RUNTIME_FORMAT_VERSION,
            execution_id: execution_id.into(),
            version: 0,
            records: Vec::new(),
        });
    // Upgrade the envelope atomically on append; version-2 history stays intact.
    entry.format_version = RUNTIME_FORMAT_VERSION;
    entry.records.extend_from_slice(records);
    entry.version = version;
    for key in keys {
        state
            .keys
            .insert((key.scope.clone(), key.key.clone()), key.clone());
    }
    Ok(version)
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod discovery_tests;
