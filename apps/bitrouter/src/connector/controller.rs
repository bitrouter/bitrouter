//! Single-owner atomic admission and durable execution transitions.

use std::{
    collections::BTreeMap,
    sync::Arc,
    time::{Duration, Instant},
};

use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use thiserror::Error;
use tokio::sync::Mutex;

use super::{
    capabilities::{Capabilities, capabilities},
    config::ConnectorConfig,
    journal::{ExecutionRecord, ExecutionState, Journal, TerminalEvidence},
    observer::PoolSnapshot,
    policy::{self, BudgetDecision, PauseReason},
};

#[derive(Debug, Error)]
pub enum ControllerError {
    #[error("connector execution configuration is invalid")]
    Configuration,
    #[error("connector execution is observation-only")]
    ObservationOnly,
    #[error("connector execution requires a supported Unix persistent filesystem")]
    UnsupportedHost,
    #[error("connector persistent state requires explicit initialization or recovery")]
    StateNotInitialized,
    #[error("connector persistent authority is already owned or unavailable")]
    OwnershipUnavailable,
    #[error("connector persistent state identity does not match this pool")]
    StoreIdentityMismatch,
    #[error("connector persistent state is unavailable; new execution is fenced")]
    Storage,
    #[error("connector request identity or bounds are invalid")]
    InvalidRequest,
    #[error("connector request id was already used with different content")]
    Conflict,
    #[error("connector request was not found")]
    NotFound,
    #[error("connector execution transition is not permitted")]
    InvalidTransition,
    #[error("connector admission refused: {0:?}")]
    Refused(PauseReason),
}

impl ControllerError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Configuration => "invalid_configuration",
            Self::ObservationOnly => "observation_only",
            Self::UnsupportedHost => "unsupported_host",
            Self::StateNotInitialized => "state_not_initialized",
            Self::OwnershipUnavailable => "ownership_unavailable",
            Self::StoreIdentityMismatch => "store_identity_mismatch",
            Self::Storage => "storage_unavailable",
            Self::InvalidRequest => "invalid_request",
            Self::Conflict => "request_id_conflict",
            Self::NotFound => "request_not_found",
            Self::InvalidTransition => "invalid_transition",
            Self::Refused(reason) => reason.code(),
        }
    }
}

#[derive(Clone, Debug)]
pub struct RequestIdentity {
    pub request_id: String,
    pub digest: String,
    pub pool_id: String,
    pub pool_generation: String,
    pub model: String,
    pub member_id: String,
    pub input_tokens: u32,
    pub max_output_tokens: u32,
    /// Absolute deadline in Unix milliseconds; HTTP callers provide a bounded
    /// duration which the ingress also enforces with a monotonic timer.
    pub deadline_ms: i64,
}

#[derive(Clone, Debug)]
pub enum Admission {
    Reserved(ExecutionRecord),
    Existing(ExecutionRecord),
}

/// Trusted operator attestation after external diagnosis, never provider
/// completion evidence. It releases occupancy without changing request identity.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct OperatorAttestation {
    pub attempt_id: String,
    pub pool_generation: String,
    pub expected_revision: u64,
    pub expected_state: ExecutionState,
    pub confirmed_not_running: bool,
    pub evidence_reference: String,
}

#[derive(Clone, Debug, Default, Serialize)]
pub struct Inflight {
    pub reserved: usize,
    pub sending: usize,
    pub running: usize,
    pub unknown: usize,
    pub total: usize,
}

#[derive(Clone, Debug, Serialize)]
pub struct ControllerStatus {
    pub pool_id: String,
    pub pool_generation: String,
    pub owner_epoch: u64,
    pub paused: bool,
    pub capabilities: Capabilities,
    pub budget: BudgetDecision,
    pub inflight: Inflight,
    pub retained_records: usize,
    pub max_records: u32,
    pub snapshot: Option<PoolSnapshot>,
}

#[derive(Clone)]
pub struct Controller {
    config: ConnectorConfig,
    generation: String,
    policy_revision: String,
    state: Arc<Mutex<State>>,
}

struct State {
    journal: Journal,
    records: BTreeMap<String, ExecutionRecord>,
    snapshot: Option<(PoolSnapshot, Instant)>,
    healthy_since: Option<Instant>,
    paused: bool,
    storage_failed: bool,
    clock_fault: bool,
    last_wall_ms: i64,
    last_tick: Instant,
    decision: BudgetDecision,
    budget_until: Instant,
    tokens: f64,
    last_refill: Instant,
}

impl Controller {
    pub async fn open(config: ConnectorConfig, initialize: bool) -> Result<Self, ControllerError> {
        config
            .validate()
            .map_err(|_| ControllerError::Configuration)?;
        let execution = config
            .execution
            .as_ref()
            .filter(|execution| execution.enabled)
            .ok_or(ControllerError::ObservationOnly)?;
        let generation = fingerprint(&config.pool)?;
        let policy_revision = fingerprint(execution)?;
        let (journal, records) =
            Journal::open(&execution.state_dir, &config.pool.id, initialize).await?;
        let now = chrono::Utc::now().timestamp_millis();
        let instant = Instant::now();
        let decision = BudgetDecision {
            pool_id: config.pool.id.clone(),
            pool_generation: generation.clone(),
            workload_class: "bounded_text_chat",
            policy_mode: "operator_limits_aggregate_plus_unfinished_guard",
            policy_revision: policy_revision.clone(),
            owner_epoch: journal.epoch,
            sequence: 0,
            decision_id: uuid::Uuid::new_v4().to_string(),
            issued_at_ms: now,
            valid_until_ms: now,
            observed_at_ms: None,
            max_dispatch_rps: 0.0,
            burst: 0,
            max_extra_inflight: 0,
            max_input_tokens: execution.max_input_tokens,
            max_output_tokens: execution.max_output_tokens,
            reason: Some(PauseReason::MissingObservation),
        };
        let paused = journal.paused;
        Ok(Self {
            config,
            generation,
            policy_revision,
            state: Arc::new(Mutex::new(State {
                journal,
                records,
                snapshot: None,
                healthy_since: None,
                paused,
                storage_failed: false,
                clock_fault: false,
                last_wall_ms: now,
                last_tick: instant,
                decision,
                budget_until: instant,
                tokens: 0.0,
                last_refill: instant,
            })),
        })
    }

    pub fn config(&self) -> &ConnectorConfig {
        &self.config
    }
    pub fn generation(&self) -> &str {
        &self.generation
    }

    pub async fn update_snapshot(
        &self,
        snapshot: PoolSnapshot,
    ) -> Result<BudgetDecision, ControllerError> {
        let mut state = self.state.lock().await;
        let now = chrono::Utc::now().timestamp_millis();
        let instant = Instant::now();
        let limits = self.limits()?;
        state.clock(now, instant);
        let previous_gap = state
            .snapshot
            .as_ref()
            .map(|(_, seen)| instant.duration_since(*seen));
        if state
            .snapshot
            .as_ref()
            .is_some_and(|(old, _)| snapshot.observed_at_ms < old.observed_at_ms)
        {
            state.clock_fault = true;
        }
        state.snapshot = Some((snapshot, instant));
        let raw_reason = self.base_reason(&state, now, instant, 0);
        if raw_reason.is_some()
            || previous_gap.is_some_and(|gap| gap > Duration::from_millis(limits.lease_ms))
        {
            state.healthy_since = None;
        }
        if raw_reason.is_none() && state.healthy_since.is_none() {
            state.healthy_since = Some(instant);
        }
        let mut reason = raw_reason;
        if reason.is_none()
            && !state.healthy_since.is_some_and(|since| {
                instant.duration_since(since) >= Duration::from_millis(limits.health_window_ms)
            })
        {
            reason = Some(PauseReason::HealthWindow);
        }
        let valid_until = now.saturating_add(limits.lease_ms as i64);
        state.decision = BudgetDecision {
            pool_id: self.config.pool.id.clone(),
            pool_generation: self.generation.clone(),
            workload_class: "bounded_text_chat",
            policy_mode: "operator_limits_aggregate_plus_unfinished_guard",
            policy_revision: self.policy_revision.clone(),
            owner_epoch: state.journal.epoch,
            sequence: state
                .decision
                .sequence
                .checked_add(1)
                .ok_or(ControllerError::Storage)?,
            decision_id: uuid::Uuid::new_v4().to_string(),
            issued_at_ms: now,
            valid_until_ms: valid_until,
            observed_at_ms: state
                .snapshot
                .as_ref()
                .map(|(snapshot, _)| snapshot.observed_at_ms),
            max_dispatch_rps: if reason.is_none() {
                limits.max_dispatch_rps
            } else {
                0.0
            },
            burst: if reason.is_none() { limits.burst } else { 0 },
            max_extra_inflight: if reason.is_none() {
                limits.max_extra_inflight
            } else {
                0
            },
            max_input_tokens: limits.max_input_tokens,
            max_output_tokens: limits.max_output_tokens,
            reason,
        };
        state.budget_until = instant + Duration::from_millis(limits.lease_ms);
        // Budget refresh never creates dispatch credits. They refill only with
        // elapsed monotonic time; a process restart starts with zero credits.
        state.refill(instant, limits.max_dispatch_rps, limits.burst);
        Ok(state.decision.clone())
    }

    pub async fn lookup(
        &self,
        id: &str,
        digest: &str,
    ) -> Result<Option<ExecutionRecord>, ControllerError> {
        let state = self.state.lock().await;
        match state.records.get(id) {
            Some(record) if record.digest != digest => Err(ControllerError::Conflict),
            record => Ok(record.cloned()),
        }
    }

    pub async fn query(&self, id: &str) -> Result<Option<ExecutionRecord>, ControllerError> {
        Ok(self.state.lock().await.records.get(id).cloned())
    }

    pub async fn reserve(&self, request: RequestIdentity) -> Result<Admission, ControllerError> {
        let mut state = self.state.lock().await;
        if let Some(existing) = state.records.get(&request.request_id) {
            return if existing.digest == request.digest {
                Ok(Admission::Existing(existing.clone()))
            } else {
                Err(ControllerError::Conflict)
            };
        }
        let now = chrono::Utc::now().timestamp_millis();
        let instant = Instant::now();
        state.clock(now, instant);
        let limits = self.limits()?;
        if !request_id(&request.request_id)
            || request.digest.len() != 64
            || !request.digest.bytes().all(|byte| byte.is_ascii_hexdigit())
            || request.pool_id != self.config.pool.id
            || request.pool_generation != self.generation
            || request.model != self.config.pool.model
            || !self
                .config
                .pool
                .members
                .iter()
                .any(|member| member.id == request.member_id)
            || request.input_tokens == 0
            || request.input_tokens > limits.max_input_tokens
            || request.max_output_tokens == 0
            || request.max_output_tokens > limits.max_output_tokens
            || request.deadline_ms <= now
            || request.deadline_ms.saturating_sub(now) as u64 > limits.max_deadline_ms
        {
            return Err(ControllerError::InvalidRequest);
        }
        self.ensure_admission(&state, now, instant, 1)?;
        if state.records.len() >= limits.max_records as usize {
            return Err(ControllerError::Refused(PauseReason::JournalFull));
        }
        if state.inflight().total >= limits.max_extra_inflight as usize {
            return Err(ControllerError::Refused(PauseReason::InflightLimit));
        }
        let record = ExecutionRecord {
            request_id: request.request_id,
            digest: request.digest,
            pool_id: request.pool_id,
            pool_generation: request.pool_generation,
            model: request.model,
            member_id: request.member_id,
            input_tokens: request.input_tokens,
            max_output_tokens: request.max_output_tokens,
            deadline_ms: request.deadline_ms,
            attempt_id: uuid::Uuid::new_v4().to_string(),
            owner_epoch: state.journal.epoch,
            state: ExecutionState::Reserved,
            revision: 0,
            evidence: None,
            operator_evidence_reference: None,
            created_at_ms: now,
            updated_at_ms: now,
        };
        let record = state.persist(record, true).await?;
        Ok(Admission::Reserved(record))
    }

    /// A returned record is the permission to issue one send. The caller must
    /// never issue an upstream request if this operation errors or is cancelled.
    pub async fn begin_send(
        &self,
        reservation: &ExecutionRecord,
    ) -> Result<ExecutionRecord, ControllerError> {
        let mut state = self.state.lock().await;
        let mut record = state.bound(reservation)?.clone();
        if record.state != ExecutionState::Reserved {
            return Err(ControllerError::InvalidTransition);
        }
        let now = chrono::Utc::now().timestamp_millis();
        let instant = Instant::now();
        state.clock(now, instant);
        let refusal = if record.deadline_ms <= now {
            Some(ControllerError::InvalidRequest)
        } else {
            self.ensure_admission(&state, now, instant, 0).err()
        };
        let limits = self.limits()?;
        state.refill(instant, limits.max_dispatch_rps, limits.burst);
        let refusal = refusal.or_else(|| {
            (state.tokens < 1.0).then_some(ControllerError::Refused(PauseReason::DispatchRate))
        });
        if let Some(error) = refusal {
            record.state = ExecutionState::Cancelled;
            record.evidence = Some(error.code().into());
            record.updated_at_ms = now;
            state.persist(record, false).await?;
            return Err(error);
        }
        record.state = ExecutionState::Sending;
        record.evidence = Some("durable_send_intent".into());
        record.updated_at_ms = now;
        let mut record = state.persist(record, false).await?;
        // Durable storage may wait longer than the observation or control
        // lease. Intent alone is not permission to dispatch: recheck at the
        // actual handoff after every awaited persistence operation.
        let now = chrono::Utc::now().timestamp_millis();
        let instant = Instant::now();
        state.clock(now, instant);
        state.refill(instant, limits.max_dispatch_rps, limits.burst);
        let refusal = if record.deadline_ms <= now {
            Some(ControllerError::InvalidRequest)
        } else {
            self.ensure_admission(&state, now, instant, 0).err()
        };
        if let Some(error) = refusal {
            // No caller has received send permission and transport has never
            // been polled, so this specific Sending intent is provably unsent.
            // A failed/cancelled compensation keeps the durable intent and
            // permanent storage fence conservative until restart recovery.
            record.state = ExecutionState::Cancelled;
            record.evidence = Some(format!("confirmed_unsent_{}", error.code()));
            record.updated_at_ms = now;
            state.persist(record, false).await?;
            return Err(error);
        }
        state.tokens -= 1.0;
        Ok(record)
    }

    #[cfg(all(test, unix))]
    pub(super) async fn hold_journal_connection(
        &self,
    ) -> Result<sea_orm::sqlx::pool::PoolConnection<sea_orm::sqlx::Sqlite>, ControllerError> {
        self.state.lock().await.journal.hold_connection().await
    }

    pub async fn mark_running(
        &self,
        handle: &ExecutionRecord,
    ) -> Result<ExecutionRecord, ControllerError> {
        self.transition(handle, ExecutionState::Running, "upstream_response_started")
            .await
    }

    pub async fn mark_unknown(
        &self,
        handle: &ExecutionRecord,
        reason: &str,
    ) -> Result<ExecutionRecord, ControllerError> {
        if !request_id(reason) {
            return Err(ControllerError::InvalidRequest);
        }
        self.transition(handle, ExecutionState::Unknown, reason)
            .await
    }

    pub async fn complete(
        &self,
        handle: &ExecutionRecord,
        evidence: TerminalEvidence,
    ) -> Result<ExecutionRecord, ControllerError> {
        let (state, reason) = match evidence {
            TerminalEvidence::Succeeded => (ExecutionState::Succeeded, "upstream_completed"),
            TerminalEvidence::Failed => (ExecutionState::Failed, "upstream_terminal_failure"),
        };
        self.transition(handle, state, reason).await
    }

    async fn transition(
        &self,
        handle: &ExecutionRecord,
        next: ExecutionState,
        evidence: &str,
    ) -> Result<ExecutionRecord, ControllerError> {
        let mut state = self.state.lock().await;
        let mut record = state.bound(handle)?.clone();
        if record.state == next {
            return Ok(record);
        }
        if record.state.is_terminal() {
            return Err(ControllerError::InvalidTransition);
        }
        let valid = match next {
            ExecutionState::Running => record.state == ExecutionState::Sending,
            ExecutionState::Unknown => matches!(
                record.state,
                ExecutionState::Sending | ExecutionState::Running
            ),
            ExecutionState::Succeeded | ExecutionState::Failed => matches!(
                record.state,
                ExecutionState::Sending | ExecutionState::Running | ExecutionState::Unknown
            ),
            _ => false,
        };
        if !valid {
            return Err(ControllerError::InvalidTransition);
        }
        record.state = next;
        record.evidence = Some(evidence.into());
        record.updated_at_ms = chrono::Utc::now().timestamp_millis();
        state.persist(record, false).await
    }

    pub async fn cancel_before_send(&self, id: &str) -> Result<ExecutionRecord, ControllerError> {
        let mut state = self.state.lock().await;
        let mut record = state
            .records
            .get(id)
            .cloned()
            .ok_or(ControllerError::NotFound)?;
        if record.state == ExecutionState::Cancelled {
            return Ok(record);
        }
        if record.state != ExecutionState::Reserved {
            return Err(ControllerError::InvalidTransition);
        }
        record.state = ExecutionState::Cancelled;
        record.evidence = Some("confirmed_unsent_cancel".into());
        record.updated_at_ms = chrono::Utc::now().timestamp_millis();
        state.persist(record, false).await
    }

    pub async fn reconcile(
        &self,
        id: &str,
        attestation: OperatorAttestation,
    ) -> Result<ExecutionRecord, ControllerError> {
        let mut state = self.state.lock().await;
        let mut record = state
            .records
            .get(id)
            .cloned()
            .ok_or(ControllerError::NotFound)?;
        if record.state != ExecutionState::Unknown
            || attestation.expected_state != ExecutionState::Unknown
            || record.revision != attestation.expected_revision
            || record.attempt_id != attestation.attempt_id
            || record.pool_generation != attestation.pool_generation
            || !attestation.confirmed_not_running
            || attestation.evidence_reference.is_empty()
            || attestation.evidence_reference.len() > 512
            || !attestation
                .evidence_reference
                .bytes()
                .all(|byte| byte.is_ascii_graphic() || byte == b' ')
        {
            return Err(ControllerError::InvalidTransition);
        }
        record.state = ExecutionState::Reconciled;
        record.evidence = Some("operator_confirmed_stopped".into());
        record.operator_evidence_reference = Some(attestation.evidence_reference);
        record.updated_at_ms = chrono::Utc::now().timestamp_millis();
        state.healthy_since = None;
        state.budget_until = Instant::now();
        state.persist(record, false).await
    }

    pub async fn pause(&self) -> Result<ControllerStatus, ControllerError> {
        let mut state = self.state.lock().await;
        state.paused = true;
        state.healthy_since = None;
        state.persist_pause(true).await?;
        self.status_locked(&mut state)
    }

    pub async fn resume(&self) -> Result<ControllerStatus, ControllerError> {
        let mut state = self.state.lock().await;
        state.persist_pause(false).await?;
        state.paused = false;
        // Resume only starts a new observation/health window. It never clears
        // unknown execution occupancy or a storage fence.
        state.clock_fault = false;
        state.last_wall_ms = chrono::Utc::now().timestamp_millis();
        state.last_tick = Instant::now();
        state.healthy_since = None;
        state.budget_until = Instant::now();
        self.status_locked(&mut state)
    }

    pub async fn status(&self) -> Result<ControllerStatus, ControllerError> {
        let mut state = self.state.lock().await;
        self.status_locked(&mut state)
    }

    fn status_locked(&self, state: &mut State) -> Result<ControllerStatus, ControllerError> {
        let now = chrono::Utc::now().timestamp_millis();
        let instant = Instant::now();
        state.clock(now, instant);
        let mut budget = state.decision.clone();
        let limits = self.limits()?;
        let reason = self
            .admission_reason(state, now, instant, 0)
            .or_else(|| {
                (state.records.len() >= limits.max_records as usize)
                    .then_some(PauseReason::JournalFull)
            })
            .or_else(|| {
                (state.inflight().total >= limits.max_extra_inflight as usize)
                    .then_some(PauseReason::InflightLimit)
            });
        if let Some(reason) = reason {
            budget.reason = Some(reason);
            budget.max_dispatch_rps = 0.0;
            budget.burst = 0;
            budget.max_extra_inflight = 0;
        }
        let mut execution_capabilities = capabilities();
        execution_capabilities.inference_dispatch = true;
        execution_capabilities.admission_control = true;
        execution_capabilities.budget_actuation = true;
        execution_capabilities.durable_execution = true;
        Ok(ControllerStatus {
            pool_id: self.config.pool.id.clone(),
            pool_generation: self.generation.clone(),
            owner_epoch: state.journal.epoch,
            paused: state.paused,
            capabilities: execution_capabilities,
            budget,
            inflight: state.inflight(),
            retained_records: state.records.len(),
            max_records: self.limits()?.max_records,
            snapshot: state
                .snapshot
                .as_ref()
                .map(|(snapshot, _)| snapshot.clone()),
        })
    }

    fn limits(&self) -> Result<&super::config::ExecutionConfig, ControllerError> {
        self.config
            .execution
            .as_ref()
            .ok_or(ControllerError::ObservationOnly)
    }

    fn base_reason(
        &self,
        state: &State,
        now: i64,
        instant: Instant,
        additional: u64,
    ) -> Option<PauseReason> {
        if state.storage_failed {
            return Some(PauseReason::StorageUnavailable);
        }
        if state.paused {
            return Some(PauseReason::ManualPause);
        }
        if state.clock_fault {
            return Some(PauseReason::ClockUntrusted);
        }
        if state.inflight().unknown > 0 {
            return Some(PauseReason::UnknownExecution);
        }
        let Some((snapshot, received)) = &state.snapshot else {
            return Some(PauseReason::MissingObservation);
        };
        // Aggregate gauges carry no per-attempt inclusion proof. Treat every
        // unfinished local attempt as potentially absent, including when one
        // member's scrape predates another member's collection completion.
        // This can tighten admission twice for reflected work; it cannot mint
        // capacity by guessing that a gauge already contains a particular send.
        let local_unfinished = state.inflight().total as u64;
        policy::pressure(
            &self.config,
            snapshot,
            now,
            instant.duration_since(*received),
            local_unfinished.saturating_add(additional),
        )
        .err()
    }

    fn admission_reason(
        &self,
        state: &State,
        now: i64,
        instant: Instant,
        additional: u64,
    ) -> Option<PauseReason> {
        self.base_reason(state, now, instant, additional)
            .or_else(|| {
                (instant >= state.budget_until || now >= state.decision.valid_until_ms)
                    .then_some(PauseReason::BudgetExpired)
            })
            .or(state.decision.reason)
    }

    fn ensure_admission(
        &self,
        state: &State,
        now: i64,
        instant: Instant,
        additional: u64,
    ) -> Result<(), ControllerError> {
        if let Some(reason) = self.admission_reason(state, now, instant, additional) {
            return Err(ControllerError::Refused(reason));
        }
        Ok(())
    }
}

impl State {
    fn clock(&mut self, now: i64, instant: Instant) {
        if now < self.last_wall_ms || instant < self.last_tick {
            self.clock_fault = true;
            self.healthy_since = None;
        }
        self.last_wall_ms = now;
        self.last_tick = instant;
    }

    fn refill(&mut self, instant: Instant, rps: f64, burst: u32) {
        self.tokens = (self.tokens + instant.duration_since(self.last_refill).as_secs_f64() * rps)
            .min(f64::from(burst));
        self.last_refill = instant;
    }

    fn inflight(&self) -> Inflight {
        let mut result = Inflight::default();
        for record in self.records.values() {
            match record.state {
                ExecutionState::Reserved => result.reserved += 1,
                ExecutionState::Sending => result.sending += 1,
                ExecutionState::Running => result.running += 1,
                ExecutionState::Unknown => result.unknown += 1,
                _ => {}
            }
        }
        result.total = result.reserved + result.sending + result.running + result.unknown;
        result
    }

    fn bound(&self, handle: &ExecutionRecord) -> Result<&ExecutionRecord, ControllerError> {
        let stored = self
            .records
            .get(&handle.request_id)
            .ok_or(ControllerError::NotFound)?;
        if stored.attempt_id != handle.attempt_id
            || stored.owner_epoch != handle.owner_epoch
            || handle.owner_epoch != self.journal.epoch
            || stored.digest != handle.digest
            || stored.pool_generation != handle.pool_generation
            || stored.member_id != handle.member_id
        {
            return Err(ControllerError::InvalidTransition);
        }
        Ok(stored)
    }

    async fn persist(
        &mut self,
        mut record: ExecutionRecord,
        insert: bool,
    ) -> Result<ExecutionRecord, ControllerError> {
        if self.storage_failed {
            return Err(ControllerError::Storage);
        }
        record.revision = if insert {
            1
        } else {
            self.records
                .get(&record.request_id)
                .ok_or(ControllerError::NotFound)?
                .revision
                .checked_add(1)
                .ok_or(ControllerError::Storage)?
        };
        // Fence before awaiting: a cancelled durable write may have committed,
        // so a future operation must not trust the old in-memory projection.
        self.storage_failed = true;
        self.journal.save(&record, insert).await?;
        self.records
            .insert(record.request_id.clone(), record.clone());
        self.storage_failed = false;
        Ok(record)
    }

    async fn persist_pause(&mut self, paused: bool) -> Result<(), ControllerError> {
        if self.storage_failed {
            return Err(ControllerError::Storage);
        }
        self.storage_failed = true;
        self.journal.set_paused(paused).await?;
        self.storage_failed = false;
        Ok(())
    }
}

fn request_id(value: &str) -> bool {
    !value.is_empty()
        && !matches!(value, "." | "..")
        && value.len() <= 128
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
}

fn fingerprint(value: &impl Serialize) -> Result<String, ControllerError> {
    let encoded = serde_json::to_vec(value).map_err(|_| ControllerError::Configuration)?;
    Ok(hex::encode(Sha256::digest(encoded)))
}
