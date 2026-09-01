//! The `requests` table — one row per settled request.

use sea_orm::entity::prelude::*;

/// One row of the `requests` table.
#[derive(Clone, Debug, PartialEq, Eq, DeriveEntityModel)]
#[sea_orm(table_name = "requests")]
pub struct Model {
    /// Unique request id.
    #[sea_orm(primary_key, auto_increment = false)]
    pub request_id: String,
    /// Owning user id.
    pub user_id: String,
    /// API key id that made the request.
    pub api_key_id: String,
    /// The `bitrouter launch` session this request belongs to, when one minted
    /// the credential it arrived with. `None` for every other caller.
    pub launch_id: Option<String>,
    /// Recognized agent harness for this request.
    pub agent_harness: Option<String>,
    /// Credential-bound ACP controller identity.
    pub controller_instance_id: Option<String>,
    /// Trusted ACP session binding.
    pub acp_session_id: Option<String>,
    /// Native harness root-session identity.
    pub native_root_session_id: Option<String>,
    /// Native exact agent/thread identity.
    pub native_agent_thread_id: Option<String>,
    /// Native parent agent/thread identity.
    pub native_parent_agent_thread_id: Option<String>,
    /// Native harness turn identity.
    pub native_turn_id: Option<String>,
    /// Applied or matched ephemeral route lease identity.
    pub route_lease_id: Option<String>,
    /// Redaction-reviewed normalized identity evidence and conflicts.
    pub session_identity_json: Option<String>,
    /// Resolved model id.
    pub model_id: String,
    /// Resolved provider id.
    pub provider_id: String,
    /// Prompt tokens consumed.
    pub prompt_tokens: i64,
    /// Completion tokens consumed.
    pub completion_tokens: i64,
    /// Reasoning tokens consumed.
    pub reasoning_tokens: i64,
    /// Cache-read prompt tokens.
    pub cache_read_tokens: i64,
    /// Cache-write prompt tokens.
    pub cache_write_tokens: i64,
    /// Uncached input tokens after normalizing cache subsets.
    pub uncached_input_tokens: i64,
    /// Non-reasoning output tokens after normalizing the reasoning subset.
    pub output_tokens: i64,
    /// Usage provenance (`provider_reported`, `authoritative_receipt`,
    /// `estimated`, or `unknown`).
    pub usage_origin: String,
    /// Verbatim provider usage object serialized as JSON.
    pub raw_usage_json: Option<String>,
    /// Charge evidence state (`computed`, `not_charged`, `unknown`, or
    /// `legacy_unknown`).
    pub charge_status: String,
    /// Full charge evidence serialized as JSON.
    pub charge_evidence_json: Option<String>,
    /// Request-scoped reconciliation state.
    pub reconciliation_status: String,
    /// Number of receipt fetches attempted.
    pub reconciliation_attempts: i32,
    /// Last content-free reconciliation error.
    pub reconciliation_last_error: Option<String>,
    /// RFC3339 timestamp of the latest receipt fetch.
    pub reconciliation_last_attempt_at: Option<String>,
    /// RFC3339 timestamp when an authoritative terminal receipt was applied.
    pub authoritative_settled_at: Option<String>,
    /// Content-free authoritative receipt serialized as JSON.
    pub authoritative_receipt_json: Option<String>,
    /// Estimated charge in micro-USD computed from pricing × tokens.
    pub estimated_charge_micro_usd: i64,
    /// Whether the request was streamed (`1`) or not (`0`).
    pub streamed: i32,
    /// End-to-end latency in milliseconds.
    pub latency_ms: i64,
    /// Upstream generation time in milliseconds.
    pub generation_time_ms: i64,
    /// Error string if the request failed, else `None`.
    pub error: Option<String>,
    /// RFC3339 creation timestamp.
    pub created_at: String,
}

#[derive(Copy, Clone, Debug, EnumIter, DeriveRelation)]
pub enum Relation {}

impl ActiveModelBehavior for ActiveModel {}
