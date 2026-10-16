//! Persisted session records and offline checkpoint inspection.
//! Scheduling and execution transitions are implemented by core.

use super::SessionSnapshot;
use super::allocation::ContextSource;
use bitrouter_ai::core::protocol::TaskInput;
use bitrouter_ai::harness::inspection::AgentStatus;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Work {
    pub text: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effort: Option<String>,
    #[serde(default)]
    pub acceptance_criteria: Vec<String>,
    #[serde(default)]
    pub required_materials: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub task_scope: Option<String>,
    #[serde(default)]
    pub fresh_context: bool,
    #[serde(default)]
    pub independent_review: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "name", content = "arguments", deny_unknown_fields)]
pub enum Action {
    #[serde(rename = "context")]
    Context(crate::storage::checkpoint::snapshot::context::action::ContextAction),
    #[serde(rename = "spawn_agent")]
    Spawn { task: Work },
    #[serde(rename = "delegate_task")]
    Delegate {
        task: Work,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        agent_id: Option<String>,
    },
    #[serde(rename = "send_message")]
    Message { agent_id: String, text: String },
    #[serde(rename = "followup_task")]
    Followup { agent_id: String, task: Work },
    #[serde(rename = "wait_agent")]
    Wait {
        agent_ids: Vec<String>,
        #[serde(default = "default_wait")]
        timeout_ms: u64,
    },
    #[serde(rename = "interrupt_agent")]
    Interrupt { agent_id: String },
    #[serde(rename = "list_agents")]
    List {},
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Mail {
    pub message_id: String,
    pub sender_id: String,
    pub sender_turn_id: String,
    pub kind: String,
    pub content: Value,
    pub context_sources: Vec<ContextSource>,
    pub consumed: bool,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Assignment {
    pub assignment_id: String,
    pub run_id: String,
    pub sender_id: String,
    pub input: TaskInput,
    pub required_instructions: Vec<String>,
    pub allocation_id: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WaitState {
    pub targets: BTreeMap<String, WaitTarget>,
    pub deadline_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WaitTarget {
    pub agent_turn_id: String,
    /// None means the accepted assignment is still queued.
    pub status: Option<AgentStatus>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct RuntimeWait {
    pub actor_id: String,
    pub state: WaitState,
    pub result: Option<Value>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Call {
    pub invocation_id: String,
    pub public_call_id: String,
    pub provider_call_id: String,
    pub step_id: String,
    pub action: Action,
    /// Frozen normal-wait delivery reservation; absent on legacy intents.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub wait_output_version: Option<u32>,
    pub wait: Option<WaitState>,
    pub result: Option<Value>,
    pub consumed: bool,
}

fn default_wait() -> u64 {
    30_000
}

pub fn subtree(state: &SessionSnapshot, root: &str) -> BTreeSet<String> {
    let mut ids = BTreeSet::from([root.to_owned()]);
    loop {
        let previous = ids.len();
        for agent in state.agents.values() {
            if agent
                .parent_id
                .as_ref()
                .is_some_and(|parent| ids.contains(parent))
            {
                ids.insert(agent.agent_id.clone());
            }
        }
        if ids.len() == previous {
            return ids;
        }
    }
}
