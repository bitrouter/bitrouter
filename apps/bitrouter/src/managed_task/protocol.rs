//! Owner-local managed-task v1. Daemon boots, native epochs and operation IDs
//! are separate identities; no request silently switches any of them.

use std::path::{Path, PathBuf};

use bitrouter_ai::types::ReasoningEffort;
use bitrouter_orchestrator::agent::ToolMode;
use bitrouter_orchestrator::core::checkpoint::DurableHead;
use bitrouter_orchestrator::core::protocol::{CoreError, OperationReceipt, OwnershipGrant};
use bitrouter_orchestrator::core::session::RootRun;
use bitrouter_orchestrator::core::skills::{SkillConsentAnswer, SkillConsentRequest};
use serde::{Deserialize, Serialize};

pub const VERSION: u32 = 1;
// Native input keeps its existing 2 MiB serialized allowance; the transport
// also accommodates ownership/selector metadata without narrowing that API.
pub(super) const MAX_COMMAND: u64 = 4 * 1024 * 1024;
// Preserve the native journal/output allowance when moving execution to IPC.
pub(super) const MAX_REPLY: u64 =
    bitrouter_orchestrator::harness::managed::store::MAX_STORE_BYTES as u64;

pub fn socket_path(control: &Path) -> PathBuf {
    control.with_extension("managed.sock")
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Submit {
    pub name: String,
    pub operation_id: String,
    pub workspace: PathBuf,
    pub prompt: String,
    pub model: Option<String>,
    pub effort: Option<ReasoningEffort>,
    pub max_output_tokens: Option<u32>,
    pub check: Option<String>,
    pub read_only: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<bitrouter_orchestrator::core::skills::SkillConstraints>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Operation {
    Hello,
    Submit(Submit),
    Status {
        name: String,
        operation_id: Option<String>,
    },
    Cancel {
        name: String,
        operation_id: String,
    },
    AnswerSkill {
        name: String,
        operation_id: String,
        answer_operation_id: String,
        answer: SkillConsentAnswer,
    },
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Command {
    pub version: u32,
    pub command_id: String,
    pub daemon_instance_id: Option<String>,
    pub operation: Operation,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Report {
    pub name: String,
    pub operation_id: Option<String>,
    pub state: String,
    pub workspace: Option<PathBuf>,
    pub mode: Option<ToolMode>,
    pub grant: Option<OwnershipGrant>,
    pub head: Option<DurableHead>,
    pub receipt: Option<OperationReceipt>,
    pub run: Option<RootRun>,
    pub agents: usize,
    pub resident: bool,
    pub released: bool,
    pub error: Option<CoreError>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub skill_consents: Vec<SkillConsentRequest>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub answer_receipt: Option<OperationReceipt>,
}

impl Report {
    pub(super) fn empty(name: String, operation_id: Option<String>, state: &str) -> Self {
        Self {
            name,
            operation_id,
            state: state.into(),
            workspace: None,
            mode: None,
            grant: None,
            head: None,
            receipt: None,
            run: None,
            agents: 0,
            resident: false,
            released: false,
            error: None,
            skill_consents: Vec::new(),
            answer_receipt: None,
        }
    }
}

#[derive(Debug, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum ResultBody {
    Hello {
        max_active_sessions: usize,
        #[serde(default)]
        skill_consent: bool,
        #[serde(default)]
        skill_constraints: bool,
    },
    Report {
        report: Box<Report>,
    },
    Error {
        error: CoreError,
    },
}

#[derive(Debug, Serialize, Deserialize)]
pub struct Reply {
    pub version: u32,
    pub command_id: Option<String>,
    pub daemon_instance_id: String,
    pub result: ResultBody,
}
