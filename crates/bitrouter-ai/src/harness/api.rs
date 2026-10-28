//! HTTP v2 harness requests and response envelopes. Authentication, workspace
//! grants, idempotency, admission and observation transport belong to the host.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

use crate::core::protocol::ModelMode;
use crate::harness::runtime::{ErrorCode, RuntimeCapabilities};
use crate::harness::thread::{ThreadHistoryPage, ThreadSnapshot, ThreadView};
use crate::harness::turn::{SteeringReceipt, TurnReceipt, TurnSnapshot};
use crate::types::ReasoningEffort;

pub const PROTOCOL_VERSION: u32 = 2;

#[derive(Clone, Serialize, Deserialize)]
pub struct CreateThreadRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub skills: Option<crate::core::skills::SkillConstraints>,
    pub workspace: PathBuf,
    pub model: String,
    #[serde(default)]
    pub model_mode: ModelMode,
    pub effort: Option<ReasoningEffort>,
    #[serde(default)]
    pub max_output_tokens: Option<u32>,
    #[serde(default)]
    pub read_only: bool,
    pub verification_command: Option<String>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnMode {
    Start,
    Enqueue,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnRequest {
    pub prompt: String,
    pub mode: TurnMode,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct InputRequest {
    #[serde(default)]
    pub skill_consent: bool,
    pub turn_id: String,
    pub request_id: String,
    pub approved: bool,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SteeringRequest {
    pub expected_turn_id: String,
    pub text: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CancelMode {
    Active,
    Queued,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CancelRequest {
    pub mode: CancelMode,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HistoryQuery {
    #[serde(default)]
    pub after: u64,
    pub cutoff: Option<u64>,
    #[serde(default = "page_limit")]
    pub limit: usize,
}

fn page_limit() -> usize {
    128
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ObserveQuery {
    pub after: Option<u64>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct CapabilitiesResponse {
    pub version: u32,
    pub runtime: RuntimeCapabilities,
    pub operations: Vec<String>,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ThreadResponse {
    pub version: u32,
    pub thread: ThreadSnapshot,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ThreadViewResponse {
    pub version: u32,
    pub view: ThreadView,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnResponse {
    pub version: u32,
    pub turn: TurnSnapshot,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct TurnReceiptResponse {
    pub version: u32,
    pub receipt: TurnReceipt,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct SteeringReceiptResponse {
    pub version: u32,
    pub receipt: SteeringReceipt,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct HistoryResponse {
    pub version: u32,
    pub history: ThreadHistoryPage,
}

#[derive(Clone, Serialize, Deserialize)]
pub struct ErrorResponse {
    pub error: String,
    pub code: ErrorCode,
}

/// An SSE `error` event carries this data without a version wrapper.
#[derive(Clone, Serialize, Deserialize)]
pub struct ObservationError {
    pub code: ErrorCode,
    pub message: String,
}

redacted_contract_debug!(
    CreateThreadRequest,
    TurnRequest,
    InputRequest,
    SteeringRequest,
    CapabilitiesResponse,
    ThreadResponse,
    ThreadViewResponse,
    TurnResponse,
    TurnReceiptResponse,
    SteeringReceiptResponse,
    HistoryResponse,
    ErrorResponse,
    ObservationError,
);
