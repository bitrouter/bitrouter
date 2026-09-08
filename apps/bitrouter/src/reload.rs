//! `AppReloader` — the daemon's config hot-reload fan-out.
//!
//! Kept in the lib (not `main.rs`) so the reload behaviour is testable
//! without spawning the binary — the same reason `commands.rs` lives here.
//!
//! Both reload paths build a fresh `Config` **in the app layer** and swap
//! it into the routing table via `ConfigRoutingTable::replace_config`.
//! Building the config here — above `bitrouter-providers` — is what lets
//! [`bitrouter_providers::apply_builtin_defaults`] fill the empty fields
//! of a built-in provider (`openai: {}`). The SDK's own
//! `RoutingTable::reload` sits *below* `bitrouter-providers` and so cannot
//! apply the catalog; routing through it on reload would leave a built-in
//! provider with an empty `api_base`, and an `auto_discover` provider
//! would then silently drop every model.

use std::collections::{BTreeSet, VecDeque};
use std::fmt;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::daemon::DaemonReloader;
use crate::policy::PolicyStore;
use chrono::{SecondsFormat, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

const MAX_MIXED_STATE_HISTORY: usize = 32;

/// Maximum time a reload may spend reading and validating all replacement
/// inputs before it changes any live participant.
pub const PREPARATION_TIMEOUT_SECONDS: u64 = 60;

/// Consistency of the runtime configuration after the last completed reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReloadConsistency {
    /// Every participant in the last completed reload reached its intended
    /// state, or a failed candidate made no live change.
    Consistent,
    /// A reload may have changed some live participants before it failed or
    /// was interrupted. Operators must inspect the participant report.
    Mixed,
}

/// A terminal reload outcome. `Unknown` is reserved for an interrupted
/// in-process attempt after mutation began; it is never evidence of failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReloadOutcome {
    Succeeded,
    Failed,
    PartiallyApplied,
    Unknown,
}

/// A live subsystem participating in a daemon reload.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReloadParticipant {
    RoutingTable,
    UpstreamTimeoutClients,
    PolicyTable,
    NamedPolicyRuntime,
    AccessPolicyStore,
}

/// What one reload participant actually did.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReloadParticipantOutcome {
    Applied,
    Unchanged,
    Failed,
    NotAttempted,
}

/// A safe, structured participant failure. Raw configuration and upstream
/// errors may carry credentials, so the externally visible message is fixed
/// and the detailed error remains in process logs only.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReloadFailure {
    pub code: String,
    pub message: String,
}

impl ReloadFailure {
    fn new(code: &str, message: &str) -> Self {
        Self {
            code: code.to_string(),
            message: message.to_string(),
        }
    }
}

/// The actual result for one reload participant.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReloadParticipantReport {
    pub participant: ReloadParticipant,
    pub outcome: ReloadParticipantOutcome,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ReloadFailure>,
}

/// A completed reload attempt. The report deliberately does not claim a
/// cross-subsystem transaction: a partial outcome identifies every participant
/// that changed before a later failure.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReloadReport {
    pub server_instance_id: String,
    pub generation: u64,
    pub outcome: ReloadOutcome,
    pub participants: Vec<ReloadParticipantReport>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub restart_required_fields: Vec<String>,
    /// RFC 3339 UTC timestamp captured when execution started.
    pub started_at: String,
    /// RFC 3339 UTC timestamp captured when this terminal report was recorded.
    pub completed_at: String,
}

impl ReloadReport {
    fn pending(instance: String, generation: u64) -> Self {
        Self {
            server_instance_id: instance,
            generation,
            outcome: ReloadOutcome::Failed,
            participants: ReloadParticipant::all()
                .into_iter()
                .map(|participant| ReloadParticipantReport {
                    participant,
                    outcome: ReloadParticipantOutcome::NotAttempted,
                    error: None,
                })
                .collect(),
            restart_required_fields: Vec::new(),
            started_at: now_rfc3339(),
            completed_at: String::new(),
        }
    }

    /// Build a successful report for coordinator-backed fakes in crate tests.
    #[cfg(test)]
    pub(crate) fn succeeded(instance: String, generation: u64) -> Self {
        let mut report = Self::pending(instance, generation);
        for participant in &mut report.participants {
            participant.outcome = ReloadParticipantOutcome::Unchanged;
        }
        report.outcome = ReloadOutcome::Succeeded;
        report.completed_at = now_rfc3339();
        report
    }

    pub(crate) fn unsupported() -> Self {
        let mut report = Self::pending("unsupported".to_string(), 0);
        report.participant_failed(
            ReloadParticipant::RoutingTable,
            ReloadFailure::new(
                "unsupported_reload",
                "this daemon does not support coordinated reload",
            ),
        );
        report.outcome = ReloadOutcome::Failed;
        report.completed_at = now_rfc3339();
        report
    }

    fn participant_mut(
        &mut self,
        participant: ReloadParticipant,
    ) -> Option<&mut ReloadParticipantReport> {
        self.participants
            .iter_mut()
            .find(|entry| entry.participant == participant)
    }

    fn participant_applied(&mut self, participant: ReloadParticipant) {
        if let Some(entry) = self.participant_mut(participant) {
            entry.outcome = ReloadParticipantOutcome::Applied;
            entry.error = None;
        }
    }

    fn participant_unchanged(&mut self, participant: ReloadParticipant) {
        if let Some(entry) = self.participant_mut(participant) {
            entry.outcome = ReloadParticipantOutcome::Unchanged;
            entry.error = None;
        }
    }

    fn participant_failed(&mut self, participant: ReloadParticipant, error: ReloadFailure) {
        if let Some(entry) = self.participant_mut(participant) {
            entry.outcome = ReloadParticipantOutcome::Failed;
            entry.error = Some(error);
        }
    }

    fn finish(&mut self, outcome: ReloadOutcome) {
        self.outcome = outcome;
        self.completed_at = now_rfc3339();
    }

    fn safe_summary(&self) -> &'static str {
        match self.outcome {
            ReloadOutcome::Succeeded => "reload succeeded",
            ReloadOutcome::Failed => "reload failed before applying every requested change",
            ReloadOutcome::PartiallyApplied => {
                "reload partially applied; inspect participant results"
            }
            ReloadOutcome::Unknown => {
                "reload outcome is unknown; inspect live state before retrying"
            }
        }
    }

    fn any_applied(&self) -> bool {
        self.participants
            .iter()
            .any(|entry| entry.outcome == ReloadParticipantOutcome::Applied)
    }
}

impl ReloadParticipant {
    fn all() -> [Self; 5] {
        [
            Self::RoutingTable,
            Self::UpstreamTimeoutClients,
            Self::PolicyTable,
            Self::NamedPolicyRuntime,
            Self::AccessPolicyStore,
        ]
    }
}

/// State exposed by the coordinator. A generation marks a terminal attempt
/// boundary; it is not an atomic snapshot of every request path.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct ReloadState {
    pub server_instance_id: String,
    pub generation: u64,
    pub running: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub running_generation: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_outcome: Option<ReloadReport>,
    pub consistency: ReloadConsistency,
    /// Bounded reports for prior mixed/unknown states in this process boot.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub mixed_state_history: Vec<ReloadReport>,
}

/// Rejection before a reload operation is admitted. These conditions leave the
/// generation unchanged and cannot mutate live configuration.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ReloadAdmissionError {
    Unsupported,
    ServerInstanceChanged,
    StaleGeneration,
    ReloadInProgress,
    GenerationExhausted,
}

impl ReloadAdmissionError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::Unsupported => "unsupported_reload",
            Self::ServerInstanceChanged => "server_instance_changed",
            Self::StaleGeneration => "stale_generation",
            Self::ReloadInProgress => "reload_in_progress",
            Self::GenerationExhausted => "generation_exhausted",
        }
    }
}

impl fmt::Display for ReloadAdmissionError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(self.code())
    }
}

impl std::error::Error for ReloadAdmissionError {}

/// A coordinator-issued capability to execute exactly one admitted reload.
/// It is intentionally non-cloneable: dropping it before mutation cancels the
/// admission, and dropping it after mutation records an unknown mixed state.
pub struct ReloadReservation {
    coordinator: Arc<ReloadCoordinator>,
    ticket: uuid::Uuid,
    server_instance_id: String,
    generation: u64,
    mutation_started: AtomicBool,
    completed: AtomicBool,
    progress: Mutex<Option<ReloadReport>>,
}

impl ReloadReservation {
    /// Boot identity this reservation was issued under.
    pub fn server_instance_id(&self) -> &str {
        &self.server_instance_id
    }

    /// The generation assigned to this attempt when it reaches a terminal
    /// report. The coordinator's public generation advances only then.
    pub fn generation(&self) -> u64 {
        self.generation
    }

    fn mark_mutation_started(&self) {
        self.mutation_started.store(true, Ordering::Release);
    }

    fn update_progress(&self, report: &ReloadReport) {
        let mut progress = match self.progress.lock() {
            Ok(progress) => progress,
            Err(poisoned) => poisoned.into_inner(),
        };
        *progress = Some(report.clone());
    }

    fn progress(&self) -> Option<ReloadReport> {
        match self.progress.lock() {
            Ok(progress) => progress.clone(),
            Err(poisoned) => poisoned.into_inner().clone(),
        }
    }

    fn belongs_to(&self, coordinator: &Arc<ReloadCoordinator>) -> bool {
        Arc::ptr_eq(&self.coordinator, coordinator)
    }
}

impl Drop for ReloadReservation {
    fn drop(&mut self) {
        if self.completed.load(Ordering::Acquire) {
            return;
        }
        if self.mutation_started.load(Ordering::Acquire) {
            self.coordinator.interrupted(self, self.progress());
        } else {
            self.coordinator.abandon(self);
        }
    }
}

#[derive(Debug)]
struct CoordinatorState {
    server_instance_id: String,
    generation: u64,
    running: Option<(uuid::Uuid, u64)>,
    last_outcome: Option<ReloadReport>,
    consistency: ReloadConsistency,
    mixed_state_history: VecDeque<ReloadReport>,
}

/// Process-local ownership of reload admission, execution state, and mixed
/// state history. It never accesses the remote operation registry: callers
/// retain their registry lock, reserve here, install their record, then run the
/// reservation after releasing that registry lock.
pub(crate) struct ReloadCoordinator {
    state: Mutex<CoordinatorState>,
}

impl ReloadCoordinator {
    pub(crate) fn new() -> Arc<Self> {
        Arc::new(Self {
            state: Mutex::new(CoordinatorState {
                server_instance_id: uuid::Uuid::new_v4().to_string(),
                generation: 0,
                running: None,
                last_outcome: None,
                consistency: ReloadConsistency::Consistent,
                mixed_state_history: VecDeque::new(),
            }),
        })
    }

    pub(crate) fn state(&self) -> ReloadState {
        let state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        ReloadState {
            server_instance_id: state.server_instance_id.clone(),
            generation: state.generation,
            running: state.running.is_some(),
            running_generation: state.running.map(|(_, generation)| generation),
            last_outcome: state.last_outcome.clone(),
            consistency: state.consistency,
            mixed_state_history: state.mixed_state_history.iter().cloned().collect(),
        }
    }

    pub(crate) fn reserve_remote(
        self: &Arc<Self>,
        expected_instance: &str,
        expected_generation: u64,
    ) -> Result<ReloadReservation, ReloadAdmissionError> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.server_instance_id != expected_instance {
            return Err(ReloadAdmissionError::ServerInstanceChanged);
        }
        if state.generation != expected_generation {
            return Err(ReloadAdmissionError::StaleGeneration);
        }
        Self::reserve_locked(self, &mut state)
    }

    pub(crate) fn reserve_local(
        self: &Arc<Self>,
    ) -> Result<ReloadReservation, ReloadAdmissionError> {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        Self::reserve_locked(self, &mut state)
    }

    fn reserve_locked(
        coordinator: &Arc<Self>,
        state: &mut CoordinatorState,
    ) -> Result<ReloadReservation, ReloadAdmissionError> {
        if state.running.is_some() {
            return Err(ReloadAdmissionError::ReloadInProgress);
        }
        let generation = state
            .generation
            .checked_add(1)
            .ok_or(ReloadAdmissionError::GenerationExhausted)?;
        let ticket = uuid::Uuid::new_v4();
        state.running = Some((ticket, generation));
        Ok(ReloadReservation {
            coordinator: Arc::clone(coordinator),
            ticket,
            server_instance_id: state.server_instance_id.clone(),
            generation,
            mutation_started: AtomicBool::new(false),
            completed: AtomicBool::new(false),
            progress: Mutex::new(None),
        })
    }

    pub(crate) fn complete(&self, reservation: &ReloadReservation, mut report: ReloadReport) {
        report.completed_at = now_rfc3339();
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.running != Some((reservation.ticket, reservation.generation)) {
            return;
        }
        state.running = None;
        state.generation = reservation.generation;
        state.consistency = match report.outcome {
            ReloadOutcome::Succeeded => ReloadConsistency::Consistent,
            ReloadOutcome::PartiallyApplied | ReloadOutcome::Unknown => ReloadConsistency::Mixed,
            // A failed candidate with no reported live mutation cannot repair
            // a previous mixed state. Keep its warning visible until a later
            // full success establishes a coherent replacement.
            ReloadOutcome::Failed => state.consistency,
        };
        if matches!(
            report.outcome,
            ReloadOutcome::PartiallyApplied | ReloadOutcome::Unknown
        ) {
            state.mixed_state_history.push_back(report.clone());
            while state.mixed_state_history.len() > MAX_MIXED_STATE_HISTORY {
                let _ = state.mixed_state_history.pop_front();
            }
        }
        state.last_outcome = Some(report);
        reservation.completed.store(true, Ordering::Release);
    }

    fn abandon(&self, reservation: &ReloadReservation) {
        let mut state = match self.state.lock() {
            Ok(state) => state,
            Err(poisoned) => poisoned.into_inner(),
        };
        if state.running == Some((reservation.ticket, reservation.generation)) {
            state.running = None;
        }
    }

    fn interrupted(&self, reservation: &ReloadReservation, progress: Option<ReloadReport>) {
        let mut report = match progress {
            Some(report) => report,
            None => ReloadReport::pending(
                reservation.server_instance_id.clone(),
                reservation.generation,
            ),
        };
        // Keep every participant result that was recorded before cancellation.
        // An interrupted attempt is overall unknown, but overwriting an earlier
        // `Applied` result would hide a live change precisely when operators
        // need to inspect the mixed runtime state.
        report.finish(ReloadOutcome::Unknown);
        self.complete(reservation, report);
    }
}

fn now_rfc3339() -> String {
    Utc::now().to_rfc3339_opts(SecondsFormat::Millis, true)
}

/// Return configuration fields that are not owned by the live reload
/// participants. This is intentionally conservative: a field is admitted only
/// when its current runtime consumer can observe a replacement without
/// rebuilding the daemon pipeline.
fn restart_required_fields(
    current: &bitrouter_sdk::config::Config,
    candidate: &bitrouter_sdk::config::Config,
    unclassified_fields: Option<&BTreeSet<String>>,
) -> Vec<String> {
    let mut fields = BTreeSet::new();
    if let Some(unclassified_fields) = unclassified_fields {
        fields.extend(unclassified_fields.iter().cloned());
    }

    if current.server.listen != candidate.server.listen {
        fields.insert("server.listen".to_string());
    }
    if current.server.control_socket != candidate.server.control_socket {
        fields.insert("server.control_socket".to_string());
    }
    if current.server.log_level != candidate.server.log_level {
        fields.insert("server.log_level".to_string());
    }
    if current.server.skip_auth != candidate.server.skip_auth {
        fields.insert("server.skip_auth".to_string());
    }
    // This switch changes whether assembly fills provider defaults and merges
    // the public registry. The reloader can refresh its routing table, but it
    // cannot rebuild every startup-owned consumer that was assembled from the
    // resulting provider set, such as auth adapters and pricing state.
    if current.inherit_defaults != candidate.inherit_defaults {
        fields.insert("inherit_defaults".to_string());
    }
    if current.control != candidate.control {
        fields.insert("control".to_string());
    }
    if current.chat != candidate.chat {
        fields.insert("chat".to_string());
    }
    if current.database.url != candidate.database.url {
        fields.insert("database.url".to_string());
    }
    if serialized_values_differ(&current.eval, &candidate.eval) {
        fields.insert("eval".to_string());
    }
    if current.trajectory != candidate.trajectory {
        fields.insert("trajectory".to_string());
    }
    if current.continuation != candidate.continuation {
        fields.insert("continuation".to_string());
    }
    if current.plugins != candidate.plugins {
        fields.insert("plugins".to_string());
    }
    if mcp_config_changed(current, candidate) {
        fields.insert("mcp".to_string());
    }
    if mcp_servers_changed(current, candidate) {
        fields.insert("mcp_servers".to_string());
    }
    if server_tools_changed(current, candidate) {
        fields.insert("server_tools".to_string());
    }
    if agents_changed(current, candidate) {
        fields.insert("agents".to_string());
    }
    if current.upstream.fallback_backoff_ms != candidate.upstream.fallback_backoff_ms {
        fields.insert("upstream.fallback_backoff_ms".to_string());
    }
    if pricing_signature(current) != pricing_signature(candidate) {
        fields.insert("providers.*.models.*.pricing".to_string());
    }

    // These providers install protocol or authentication adapters during
    // assembly. Their routing entries can change, but adding or removing the
    // adapter-bearing provider itself needs a rebuilt executor.
    for provider_id in [
        "bitrouter",
        "github-copilot",
        "anthropic",
        "claude-code",
        "openai-codex",
        "supergrok",
        bitrouter_providers::antigravity::PROVIDER_ID,
    ] {
        if current.providers.contains_key(provider_id)
            != candidate.providers.contains_key(provider_id)
        {
            fields.insert(format!("providers.{provider_id}"));
        }
    }

    fields.into_iter().collect()
}

/// Return paths present in the raw server-owned candidate that the current
/// configuration schema does not describe. The SDK deliberately accepts
/// unknown fields for forward-compatible local files, but remote reload must
/// be conservative: an ignored nested field is not proof that a live consumer
/// can apply it. Dynamic maps remain open only where their schema explicitly
/// says what their values are (for example `providers.<id>`).
fn unclassified_config_paths(value: &serde_json::Value) -> BTreeSet<String> {
    let schema = schemars::schema_for!(bitrouter_sdk::config::Config);
    let root = schema.as_value();
    let mut fields = BTreeSet::new();
    collect_unclassified_schema_paths(value, root, root, "", &mut fields);
    fields
}

fn collect_unclassified_schema_paths(
    value: &serde_json::Value,
    schema: &serde_json::Value,
    root: &serde_json::Value,
    path: &str,
    fields: &mut BTreeSet<String>,
) {
    let Some(schema) = resolve_schema(schema, root) else {
        if !path.is_empty() {
            fields.insert(path.to_string());
        }
        return;
    };
    if schema.as_bool() == Some(false) {
        if !path.is_empty() {
            fields.insert(path.to_string());
        }
        return;
    }
    if schema.as_bool() == Some(true) {
        return;
    }
    if !schema_accepts_value_kind(schema, value) {
        if !path.is_empty() {
            fields.insert(path.to_string());
        }
        return;
    }

    for union in ["anyOf", "oneOf"] {
        if let Some(variants) = schema.get(union).and_then(serde_json::Value::as_array) {
            let mut best: Option<BTreeSet<String>> = None;
            for variant in variants {
                let mut candidate = BTreeSet::new();
                collect_unclassified_schema_paths(value, variant, root, path, &mut candidate);
                if candidate.is_empty() {
                    return;
                }
                if best
                    .as_ref()
                    .is_none_or(|current| candidate.len() < current.len())
                {
                    best = Some(candidate);
                }
            }
            if let Some(best) = best {
                fields.extend(best);
            }
            return;
        }
    }

    let all_of = schema.get("allOf").and_then(serde_json::Value::as_array);
    let has_direct_object_shape = schema.get("properties").is_some()
        || schema.get("additionalProperties").is_some()
        || schema.get("unevaluatedProperties").is_some()
        || schema.get("patternProperties").is_some();

    if !(all_of.is_some() && !has_direct_object_shape) {
        match value {
            serde_json::Value::Object(values) => {
                let properties = schema
                    .get("properties")
                    .and_then(serde_json::Value::as_object);
                let additional = schema
                    .get("additionalProperties")
                    .or_else(|| schema.get("unevaluatedProperties"));
                for (name, child) in values {
                    let child_path = schema_child_path(path, name);
                    if let Some(property) = schema_property(properties, path, name) {
                        collect_unclassified_schema_paths(
                            child,
                            property,
                            root,
                            &child_path,
                            fields,
                        );
                    } else if let Some(additional) = additional {
                        if additional.as_bool() == Some(false) {
                            fields.insert(child_path);
                        } else if additional.as_bool() != Some(true) {
                            collect_unclassified_schema_paths(
                                child,
                                additional,
                                root,
                                &child_path,
                                fields,
                            );
                        }
                    } else {
                        fields.insert(child_path);
                    }
                }
            }
            serde_json::Value::Array(values) => {
                if let Some(items) = schema.get("items") {
                    for (index, child) in values.iter().enumerate() {
                        let child_path = format!("{path}[{index}]");
                        collect_unclassified_schema_paths(child, items, root, &child_path, fields);
                    }
                }
            }
            _ => {}
        }
    }

    if let Some(all_of) = all_of {
        for part in all_of {
            collect_unclassified_schema_paths(value, part, root, path, fields);
        }
    }
}

fn schema_property<'a>(
    properties: Option<&'a serde_json::Map<String, serde_json::Value>>,
    parent_path: &str,
    name: &str,
) -> Option<&'a serde_json::Value> {
    let properties = properties?;
    if let Some(property) = properties.get(name) {
        return Some(property);
    }
    // This accepted serde alias predates remote administration. Schemars
    // documents the canonical `mode` name only, but the parser gives
    // `policy.writeback` the exact same live meaning.
    if parent_path == "policy" && name == "writeback" {
        return properties.get("mode");
    }
    None
}

fn resolve_schema<'a>(
    schema: &'a serde_json::Value,
    root: &'a serde_json::Value,
) -> Option<&'a serde_json::Value> {
    match schema.get("$ref").and_then(serde_json::Value::as_str) {
        Some(reference) => reference
            .strip_prefix('#')
            .and_then(|pointer| root.pointer(pointer)),
        None => Some(schema),
    }
}

fn schema_child_path(parent: &str, child: &str) -> String {
    if parent.is_empty() {
        child.to_string()
    } else {
        format!("{parent}.{child}")
    }
}

fn schema_accepts_value_kind(schema: &serde_json::Value, value: &serde_json::Value) -> bool {
    let Some(types) = schema.get("type") else {
        return true;
    };
    match types {
        serde_json::Value::String(kind) => json_value_matches_kind(value, kind),
        serde_json::Value::Array(kinds) => kinds.iter().any(|kind| {
            kind.as_str()
                .is_some_and(|kind| json_value_matches_kind(value, kind))
        }),
        _ => true,
    }
}

fn json_value_matches_kind(value: &serde_json::Value, kind: &str) -> bool {
    match kind {
        "object" => value.is_object(),
        "array" => value.is_array(),
        "string" => value.is_string(),
        "boolean" => value.is_boolean(),
        "null" => value.is_null(),
        "number" => value.is_number(),
        "integer" => value.as_i64().is_some() || value.as_u64().is_some(),
        _ => true,
    }
}

fn serialized_values_differ<T: Serialize>(current: &T, candidate: &T) -> bool {
    match (
        serde_json::to_value(current),
        serde_json::to_value(candidate),
    ) {
        (Ok(current), Ok(candidate)) => current != candidate,
        // Serialization is local and deterministic for these schema types. A
        // future failure means the field's consumer has changed, so reject the
        // remote attempt rather than claim the field is reloadable.
        _ => true,
    }
}

fn mcp_config_changed(
    current: &bitrouter_sdk::config::Config,
    candidate: &bitrouter_sdk::config::Config,
) -> bool {
    let current = &current.mcp;
    let candidate = &candidate.mcp;
    current.aggregate.enabled != candidate.aggregate.enabled
        || current.aggregate.route != candidate.aggregate.route
        || current.cache.enabled != candidate.cache.enabled
        || current.cache.tools_list_ttl_secs != candidate.cache.tools_list_ttl_secs
        || current.cache.resources_list_ttl_secs != candidate.cache.resources_list_ttl_secs
        || current.cache.resources_templates_list_ttl_secs
            != candidate.cache.resources_templates_list_ttl_secs
        || current.cache.prompts_list_ttl_secs != candidate.cache.prompts_list_ttl_secs
        || current.cache.max_entries_per_server != candidate.cache.max_entries_per_server
        || current.upstream_protocol != candidate.upstream_protocol
}

fn mcp_servers_changed(
    current: &bitrouter_sdk::config::Config,
    candidate: &bitrouter_sdk::config::Config,
) -> bool {
    current.mcp_servers.len() != candidate.mcp_servers.len()
        || current.mcp_servers.iter().any(|(id, current_server)| {
            candidate
                .mcp_servers
                .get(id)
                .is_none_or(|candidate_server| {
                    current_server.name != candidate_server.name
                        || current_server.transport != candidate_server.transport
                        || current_server.aggregate != candidate_server.aggregate
                        || current_server.tool_prefix != candidate_server.tool_prefix
                })
        })
}

fn agents_changed(
    current: &bitrouter_sdk::config::Config,
    candidate: &bitrouter_sdk::config::Config,
) -> bool {
    current.agents.len() != candidate.agents.len()
        || current.agents.iter().any(|(id, current_agent)| {
            candidate.agents.get(id).is_none_or(|candidate_agent| {
                current_agent.name != candidate_agent.name
                    || current_agent.transport != candidate_agent.transport
            })
        })
}

fn server_tools_changed(
    current: &bitrouter_sdk::config::Config,
    candidate: &bitrouter_sdk::config::Config,
) -> bool {
    // This comparison never leaves the process. Several tool settings contain
    // credential-bearing values but do not implement `Serialize`; `Debug` is
    // only used to compare the in-memory values and is never logged or exposed.
    format!("{:?}", current.server_tools) != format!("{:?}", candidate.server_tools)
}

fn pricing_signature(config: &bitrouter_sdk::config::Config) -> Vec<String> {
    let mut entries = Vec::new();
    for (provider_id, provider) in &config.providers {
        for model in &provider.models {
            if let Some(pricing) = &model.pricing {
                let provider_model_id = model.provider_model_id.as_deref().unwrap_or_default();
                entries.push(format!(
                    "{provider_id}|{}|{}|{pricing:?}",
                    model.id, provider_model_id
                ));
            }
        }
    }
    entries.sort();
    entries
}

/// Re-activate providers that have a credential in the OAuth store, loading the
/// default store. Best-effort: an unreadable store is a no-op. Mirrors the
/// startup pass in `assemble.rs` so a subscription / Claude Code session
/// provider survives a hot-reload instead of dropping out of routing.
fn activate_stored_credential_providers(config: &mut bitrouter_sdk::config::Config) {
    if let Ok(store) = bitrouter_providers::oauth::credential_store::CredentialStore::default_path()
    {
        bitrouter_providers::activate_stored_credential_providers(config, &store);
    }
}

/// Apply the same non-mutating configuration enrichment used when the daemon
/// constructs a replacement routing snapshot. Keeping this in one helper makes
/// the current and candidate shapes comparable before remote classification.
async fn resolve_reloadable_config(config: &mut bitrouter_sdk::config::Config) {
    bitrouter_providers::apply_builtin_defaults(config);
    crate::claude_code::enable_if_logged_in(config);
    crate::assemble::merge_registry_into(config).await;
    activate_stored_credential_providers(config);
    // Discovery is bounded by the SDK and completes before any live swap.
    bitrouter_sdk::config::discover_models(config).await;
}

/// Whether the daemon is running against a `bitrouter.yaml` on disk
/// (re-readable on reload) or a zero-config in-memory default
/// (rebuilt by re-running [`bitrouter_providers::zero_config`]).
pub enum ReloadSource {
    /// File-backed; the reloader re-reads the `bitrouter.yaml` at this
    /// path (re-substituting `${VAR}` references), re-applies the
    /// built-in provider catalog, and swaps the result into the
    /// routing table.
    File(PathBuf),
    /// In-memory zero-config; the reloader rebuilds the Config from
    /// scratch and hands it to the routing table via `replace_config`.
    Default,
}

/// Fan out a daemon `Reload` (and SIGHUP) to every reloadable subsystem the
/// running daemon owns. Failures from any single subsystem are accumulated and
/// reported together so an unrelated subsystem (e.g. a missing policy dir)
/// doesn't mask a fixable routing-table reload.
pub struct AppReloader {
    policy_store: Arc<PolicyStore>,
    /// Concrete handle on the routing table. Both reload paths build a
    /// fresh `Config` in the app layer — so `bitrouter_providers`'
    /// built-in catalog can be applied above the SDK — and swap it in
    /// via `ConfigRoutingTable::replace_config`.
    routing_table: Arc<bitrouter_sdk::config::ConfigRoutingTable>,
    /// The fully assembled configuration at daemon startup. Local reloads may
    /// refresh the routing snapshot with fields whose actual consumers were
    /// wired only at startup; remote admission must keep comparing those
    /// fields to this immutable baseline rather than accepting that stale
    /// local snapshot as evidence that they became live-reloadable.
    startup_config: bitrouter_sdk::config::Config,
    /// Concrete upstream HTTP executor. Timeout knobs are client-level, so a
    /// config reload must rebuild the live executor's client set too.
    upstream_executor: Arc<bitrouter_sdk::language_model::HttpExecutor>,
    policy_runtime: Option<Arc<crate::policy_lock::PolicyRuntime>>,
    /// The live `policy_table:` transform, when one was wired at assembly.
    /// Reload rebuilds its spec from the fresh config and swaps it in —
    /// without this the daemon kept serving the tiers it started with, and
    /// only `restart` applied an edit.
    policy_table_router: Option<Arc<crate::policy_table_router::PolicyTableRouter>>,
    /// Owns admission for local IPC, SIGHUP, and guarded remote reloads. The
    /// routing table's own lock is narrower and cannot protect preparation or
    /// the other live participants.
    coordinator: Arc<ReloadCoordinator>,
    source: ReloadSource,
    /// Test-only boundary fault. Production builds have no way to configure
    /// this; it exists so the coordinator's truthful partial-state reporting
    /// can be exercised without adding a runtime fault-injection surface.
    #[cfg(test)]
    test_apply_failure: Option<ReloadParticipant>,
    #[cfg(test)]
    test_pause_after_prepare: Option<Arc<TestPreparationPause>>,
    #[cfg(test)]
    test_pause_during_prepare: Option<Arc<TestPreparationPause>>,
    #[cfg(test)]
    test_preparation_timeout: Option<Duration>,
}

#[derive(Clone, Copy)]
enum ReloadInvocation {
    Local,
    Remote,
}

#[cfg(test)]
struct TestPreparationPause {
    prepared: tokio::sync::Notify,
    resume: tokio::sync::Notify,
}

#[cfg(test)]
impl TestPreparationPause {
    fn new() -> Self {
        Self {
            prepared: tokio::sync::Notify::new(),
            resume: tokio::sync::Notify::new(),
        }
    }
}

struct PreparedReload {
    config: bitrouter_sdk::config::Config,
    timeout_clients: bitrouter_sdk::language_model::executor::PreparedProviderTimeouts,
    policy_table: PreparedPolicyTable,
    named_policy_runtime: Option<crate::policy_lock::PreparedPolicySnapshot>,
    access_policy_store: Option<crate::policy::store::PreparedPolicyStore>,
}

enum PreparedPolicyTable {
    Apply(Arc<crate::policy_table_router::PolicyTable>),
    Unchanged,
}

struct PreparationError {
    participant: ReloadParticipant,
    failure: ReloadFailure,
    restart_required_fields: Vec<String>,
}

impl PreparationError {
    fn failed(participant: ReloadParticipant, code: &str, message: &str) -> Self {
        Self {
            participant,
            failure: ReloadFailure::new(code, message),
            restart_required_fields: Vec::new(),
        }
    }

    fn restart_required(fields: Vec<String>) -> Self {
        Self {
            participant: ReloadParticipant::RoutingTable,
            failure: ReloadFailure::new(
                "restart_required",
                "candidate changes a field that is only applied at daemon startup",
            ),
            restart_required_fields: fields,
        }
    }
}

impl AppReloader {
    /// Build a reloader over the daemon's reloadable subsystems.
    pub fn new(
        policy_store: Arc<PolicyStore>,
        routing_table: Arc<bitrouter_sdk::config::ConfigRoutingTable>,
        upstream_executor: Arc<bitrouter_sdk::language_model::HttpExecutor>,
        source: ReloadSource,
    ) -> Self {
        let startup_config = routing_table.snapshot_config();
        Self {
            policy_store,
            routing_table,
            startup_config,
            upstream_executor,
            policy_runtime: None,
            policy_table_router: None,
            coordinator: ReloadCoordinator::new(),
            source,
            #[cfg(test)]
            test_apply_failure: None,
            #[cfg(test)]
            test_pause_after_prepare: None,
            #[cfg(test)]
            test_pause_during_prepare: None,
            #[cfg(test)]
            test_preparation_timeout: None,
        }
    }

    /// Attach the live `policy_table:` transform so a reload re-applies its
    /// tiers.
    pub fn with_policy_table_router(
        mut self,
        router: Option<Arc<crate::policy_table_router::PolicyTableRouter>>,
    ) -> Self {
        self.policy_table_router = router;
        self
    }

    #[cfg(test)]
    fn with_test_apply_failure(mut self, participant: ReloadParticipant) -> Self {
        self.test_apply_failure = Some(participant);
        self
    }

    #[cfg(test)]
    fn with_test_pause_after_prepare(mut self, pause: Arc<TestPreparationPause>) -> Self {
        self.test_pause_after_prepare = Some(pause);
        self
    }

    #[cfg(test)]
    fn with_test_pause_during_prepare(mut self, pause: Arc<TestPreparationPause>) -> Self {
        self.test_pause_during_prepare = Some(pause);
        self
    }

    #[cfg(test)]
    fn with_test_preparation_timeout(mut self, timeout: Duration) -> Self {
        self.test_preparation_timeout = Some(timeout);
        self
    }

    fn should_fail_apply(&self, participant: ReloadParticipant) -> bool {
        #[cfg(test)]
        {
            self.test_apply_failure == Some(participant)
        }
        #[cfg(not(test))]
        {
            let _ = participant;
            false
        }
    }

    async fn wait_after_prepare(&self) {
        #[cfg(test)]
        if let Some(pause) = &self.test_pause_after_prepare {
            pause.prepared.notify_one();
            pause.resume.notified().await;
        }
    }

    async fn wait_during_prepare(&self) {
        #[cfg(test)]
        if let Some(pause) = &self.test_pause_during_prepare {
            pause.prepared.notify_one();
            pause.resume.notified().await;
        }
    }

    fn preparation_timeout(&self) -> Duration {
        #[cfg(test)]
        if let Some(timeout) = self.test_preparation_timeout {
            return timeout;
        }
        Duration::from_secs(PREPARATION_TIMEOUT_SECONDS)
    }

    /// Build a replacement policy table before any live participant changes.
    /// A daemon that started without this transform cannot add it later because
    /// the transform is part of the built pipeline, so remote reload correctly
    /// classifies that shape as restart-required.
    fn prepare_policy_table(
        &self,
        fresh: &bitrouter_sdk::config::Config,
    ) -> Result<PreparedPolicyTable, PreparationError> {
        let mut effective = fresh.policy_table.clone();
        effective.adequacy = fresh
            .policy
            .mode
            .apply_to_adequacy(&fresh.policy_table.adequacy);
        let candidate = match crate::policy_table_router::PolicyTable::from_config(&effective) {
            Some(table) => table,
            None => crate::policy_table_router::PolicyTable::inert(),
        };
        match (&self.policy_table_router, effective.tiers.is_empty()) {
            (Some(_), _) => Ok(PreparedPolicyTable::Apply(candidate)),
            (None, true) => Ok(PreparedPolicyTable::Unchanged),
            (None, false) => Err(PreparationError::restart_required(vec![
                "policy_table".to_string(),
            ])),
        }
    }

    async fn prepare_candidate(
        &self,
    ) -> Result<(bitrouter_sdk::config::Config, Option<BTreeSet<String>>), PreparationError> {
        let (mut fresh, fields) = match &self.source {
            ReloadSource::File(path) => {
                let raw = tokio::fs::read_to_string(path).await.map_err(|error| {
                    tracing::warn!(error = %error, "reload could not read configuration candidate");
                    PreparationError::failed(
                        ReloadParticipant::RoutingTable,
                        "config_prepare_failed",
                        "reload could not read the server configuration",
                    )
                })?;
                let config = bitrouter_sdk::config::parse_with(
                    &raw,
                    bitrouter_sdk::config::env_lookup,
                )
                .map_err(|error| {
                    tracing::warn!(error = %error, "reload configuration candidate is invalid");
                    PreparationError::failed(
                        ReloadParticipant::RoutingTable,
                        "config_prepare_failed",
                        "reload configuration is invalid",
                    )
                })?;
                let substituted = bitrouter_sdk::config::substitute_env(&raw).map_err(|error| {
                    tracing::warn!(error = %error, "reload configuration substitution failed");
                    PreparationError::failed(
                        ReloadParticipant::RoutingTable,
                        "config_prepare_failed",
                        "reload configuration is invalid",
                    )
                })?;
                let value =
                    serde_saphyr::from_str::<serde_json::Value>(&substituted).map_err(|error| {
                        tracing::warn!(error = %error, "reload configuration shape is invalid");
                        PreparationError::failed(
                            ReloadParticipant::RoutingTable,
                            "config_prepare_failed",
                            "reload configuration is invalid",
                        )
                    })?;
                if !value.is_object() {
                    return Err(PreparationError::failed(
                        ReloadParticipant::RoutingTable,
                        "config_prepare_failed",
                        "reload configuration must be a mapping",
                    ));
                }
                (config, Some(unclassified_config_paths(&value)))
            }
            ReloadSource::Default => {
                let mut config = bitrouter_providers::zero_config();
                crate::cloud::enable_in_zero_config(&mut config);
                (config, None)
            }
        };

        // Discovery is bounded by the SDK and runs here rather than during the
        // live routing-table swap. Every later participant consumes this exact
        // prepared candidate.
        resolve_reloadable_config(&mut fresh).await;
        Ok((fresh, fields))
    }

    async fn prepare(
        &self,
        invocation: ReloadInvocation,
    ) -> Result<PreparedReload, PreparationError> {
        self.wait_during_prepare().await;
        let (config, unclassified_fields) = self.prepare_candidate().await?;
        if matches!(invocation, ReloadInvocation::Remote) {
            let restart_required = restart_required_fields(
                &self.startup_config,
                &config,
                unclassified_fields.as_ref(),
            );
            if !restart_required.is_empty() {
                return Err(PreparationError::restart_required(restart_required));
            }
        }
        let policy_table = self.prepare_policy_table(&config)?;
        let named_policy_runtime = match &self.policy_runtime {
            Some(runtime) => {
                let path = match &self.source {
                    ReloadSource::File(path) => Some(path.as_path()),
                    ReloadSource::Default => None,
                };
                Some(runtime.prepare_for_config(&config, path).await.map_err(|error| {
                    tracing::warn!(error = %error, "reload named policy preparation failed");
                    PreparationError::failed(
                        ReloadParticipant::NamedPolicyRuntime,
                        "policy_prepare_failed",
                        "named policy preparation failed",
                    )
                })?)
            }
            None => None,
        };
        let (global_timeouts, provider_timeouts) =
            crate::assemble::resolved_upstream_timeouts(&config);
        let timeout_clients = self
            .upstream_executor
            .prepare_provider_timeouts(global_timeouts, provider_timeouts)
            .map_err(|error| {
                tracing::warn!(error = %error, "reload timeout client preparation failed");
                PreparationError::failed(
                    ReloadParticipant::UpstreamTimeoutClients,
                    "timeout_prepare_failed",
                    "upstream timeout client preparation failed",
                )
            })?;
        let access_policy_store = self.policy_store.prepare_reload().await.map_err(|error| {
            tracing::warn!(error = %error, "reload access-policy preparation failed");
            PreparationError::failed(
                ReloadParticipant::AccessPolicyStore,
                "access_policy_prepare_failed",
                "access-policy preparation failed",
            )
        })?;
        Ok(PreparedReload {
            config,
            timeout_clients,
            policy_table,
            named_policy_runtime,
            access_policy_store,
        })
    }

    async fn execute_reservation(
        &self,
        reservation: ReloadReservation,
        invocation: ReloadInvocation,
        env: Vec<(String, String)>,
    ) -> ReloadReport {
        let mut report = ReloadReport::pending(
            reservation.server_instance_id.clone(),
            reservation.generation,
        );
        reservation.update_progress(&report);

        // A reservation is a private coordinator capability. Refuse to run it
        // against another app instance before an override is installed or any
        // candidate input is read; dropping it then clears its original
        // coordinator's admission.
        if !reservation.belongs_to(&self.coordinator) {
            report.participant_failed(
                ReloadParticipant::RoutingTable,
                ReloadFailure::new(
                    "invalid_reservation",
                    "reload reservation does not belong to this daemon",
                ),
            );
            report.finish(ReloadOutcome::Failed);
            reservation.update_progress(&report);
            return report;
        }

        // The reservation excludes every other local, remote, and SIGHUP
        // reload. Installing the owner-trusted IPC override here keeps it in
        // that same critical section; remote requests never pass an override.
        if matches!(invocation, ReloadInvocation::Local) && !env.is_empty() {
            let overrides = env.into_iter().collect();
            bitrouter_sdk::config::set_env_overrides(overrides);
            tracing::info!("env override map updated by local reload");
        }

        let prepared = match tokio::time::timeout(
            self.preparation_timeout(),
            self.prepare(invocation),
        )
        .await
        {
            Ok(Ok(prepared)) => prepared,
            Ok(Err(error)) => {
                report.participant_failed(error.participant, error.failure);
                report.restart_required_fields = error.restart_required_fields;
                report.finish(ReloadOutcome::Failed);
                reservation.update_progress(&report);
                self.coordinator.complete(&reservation, report.clone());
                return report;
            }
            Err(_) => {
                report.participant_failed(
                    ReloadParticipant::RoutingTable,
                    ReloadFailure::new(
                        "preparation_timed_out",
                        "reload preparation exceeded its time limit",
                    ),
                );
                report.finish(ReloadOutcome::Failed);
                reservation.update_progress(&report);
                self.coordinator.complete(&reservation, report.clone());
                return report;
            }
        };

        // Test-only seam verifies that every later participant consumes these
        // objects even if the source files change after preparation.
        self.wait_after_prepare().await;
        reservation.mark_mutation_started();
        if self.should_fail_apply(ReloadParticipant::RoutingTable) {
            report.participant_failed(
                ReloadParticipant::RoutingTable,
                ReloadFailure::new("fault_injected", "routing table update failed"),
            );
        } else {
            match self
                .routing_table
                .replace_prepared_config(prepared.config)
                .await
            {
                Ok(()) => report.participant_applied(ReloadParticipant::RoutingTable),
                Err(error) => {
                    tracing::warn!(error = %error, "reload routing table swap failed");
                    report.participant_failed(
                        ReloadParticipant::RoutingTable,
                        ReloadFailure::new("routing_apply_failed", "routing table update failed"),
                    );
                }
            }
        }
        reservation.update_progress(&report);

        if self.should_fail_apply(ReloadParticipant::UpstreamTimeoutClients) {
            report.participant_failed(
                ReloadParticipant::UpstreamTimeoutClients,
                ReloadFailure::new("fault_injected", "upstream timeout client update failed"),
            );
        } else {
            self.upstream_executor
                .commit_provider_timeouts(prepared.timeout_clients);
            report.participant_applied(ReloadParticipant::UpstreamTimeoutClients);
        }
        reservation.update_progress(&report);

        if self.should_fail_apply(ReloadParticipant::PolicyTable) {
            report.participant_failed(
                ReloadParticipant::PolicyTable,
                ReloadFailure::new("fault_injected", "policy table update failed"),
            );
        } else {
            match prepared.policy_table {
                PreparedPolicyTable::Apply(table) => match &self.policy_table_router {
                    Some(router) if router.replace_table(table) => {
                        report.participant_applied(ReloadParticipant::PolicyTable);
                    }
                    Some(_) => report.participant_failed(
                        ReloadParticipant::PolicyTable,
                        ReloadFailure::new(
                            "policy_table_apply_failed",
                            "policy table update failed",
                        ),
                    ),
                    None => report.participant_unchanged(ReloadParticipant::PolicyTable),
                },
                PreparedPolicyTable::Unchanged => {
                    report.participant_unchanged(ReloadParticipant::PolicyTable);
                }
            }
        }
        reservation.update_progress(&report);

        if self.should_fail_apply(ReloadParticipant::NamedPolicyRuntime) {
            report.participant_failed(
                ReloadParticipant::NamedPolicyRuntime,
                ReloadFailure::new("fault_injected", "named policy runtime update failed"),
            );
        } else {
            match (self.policy_runtime.as_ref(), prepared.named_policy_runtime) {
                (Some(runtime), Some(prepared_runtime)) => {
                    runtime.commit(prepared_runtime);
                    report.participant_applied(ReloadParticipant::NamedPolicyRuntime);
                }
                _ => report.participant_unchanged(ReloadParticipant::NamedPolicyRuntime),
            }
        }
        reservation.update_progress(&report);

        if self.should_fail_apply(ReloadParticipant::AccessPolicyStore) {
            report.participant_failed(
                ReloadParticipant::AccessPolicyStore,
                ReloadFailure::new("fault_injected", "access-policy store update failed"),
            );
        } else {
            match prepared.access_policy_store {
                Some(prepared_store) => match self.policy_store.commit_prepared(prepared_store) {
                    Ok(()) => report.participant_applied(ReloadParticipant::AccessPolicyStore),
                    Err(error) => {
                        tracing::warn!(error = %error, "reload access-policy store swap failed");
                        report.participant_failed(
                            ReloadParticipant::AccessPolicyStore,
                            ReloadFailure::new(
                                "access_policy_apply_failed",
                                "access-policy store update failed",
                            ),
                        );
                    }
                },
                None => report.participant_unchanged(ReloadParticipant::AccessPolicyStore),
            }
        }

        let failed = report
            .participants
            .iter()
            .any(|entry| entry.outcome == ReloadParticipantOutcome::Failed);
        let outcome = if !failed {
            ReloadOutcome::Succeeded
        } else if report.any_applied() {
            ReloadOutcome::PartiallyApplied
        } else {
            ReloadOutcome::Failed
        };
        report.finish(outcome);
        reservation.update_progress(&report);
        self.coordinator.complete(&reservation, report.clone());
        report
    }

    /// Attach the app's named routing-policy registry to the same reload fanout.
    pub fn with_policy_runtime(mut self, runtime: Arc<crate::policy_lock::PolicyRuntime>) -> Self {
        self.policy_runtime = Some(runtime);
        self
    }
}

#[async_trait::async_trait]
impl DaemonReloader for AppReloader {
    async fn reload(&self) -> anyhow::Result<()> {
        self.reload_with_env(Vec::new()).await
    }

    async fn reload_with_env(&self, env: Vec<(String, String)>) -> anyhow::Result<()> {
        let reservation = self
            .coordinator
            .reserve_local()
            .map_err(anyhow::Error::from)?;
        let report = self
            .execute_reservation(reservation, ReloadInvocation::Local, env)
            .await;
        if report.outcome == ReloadOutcome::Succeeded {
            Ok(())
        } else {
            Err(anyhow::anyhow!(report.safe_summary()))
        }
    }

    fn reload_state(&self) -> Option<ReloadState> {
        Some(self.coordinator.state())
    }

    fn reserve_remote(
        &self,
        expected_instance: &str,
        expected_generation: u64,
    ) -> Result<ReloadReservation, ReloadAdmissionError> {
        self.coordinator
            .reserve_remote(expected_instance, expected_generation)
    }

    async fn reload_reserved(&self, reservation: ReloadReservation) -> ReloadReport {
        self.execute_reservation(reservation, ReloadInvocation::Remote, Vec::new())
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bitrouter_sdk::config::{self, ConfigRoutingTable};
    use bitrouter_sdk::language_model::{
        ApiProtocol, Executor, GenerationParams, HttpExecutor, HttpTimeouts, Message,
        PipelineContext, PipelineRequest, Prompt, Role, RoutingTarget,
    };
    use std::time::Duration;
    use tokio::io::{AsyncReadExt, AsyncWriteExt};

    fn temp_config_path() -> (std::path::PathBuf, std::path::PathBuf) {
        let dir = tempfile::Builder::new()
            .prefix("bitrouter-reload-")
            .tempdir()
            .expect("create temp config dir")
            .keep();
        (dir.join("bitrouter.yaml"), dir)
    }

    fn config_yaml(read_secs: u64) -> String {
        format!(
            r#"
inherit_defaults: false
upstream:
  timeouts:
    read_secs: {read_secs}
providers:
  slow:
    api_base: https://api.example.com/v1
    api_key: k
    api_protocol:
      - "*": chat_completions
    models:
      - {{ id: m }}
"#
        )
    }

    async fn stalled_json_server() -> String {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind test server");
        let addr = listener.local_addr().expect("test server addr");
        tokio::spawn(async move {
            let (mut socket, _) = listener.accept().await.expect("accept request");
            let mut request_buf = [0_u8; 1024];
            let _ = socket.read(&mut request_buf).await;
            socket
                .write_all(
                    b"HTTP/1.1 200 OK\r\n\
                      content-type: application/json\r\n\
                      content-length: 1024\r\n\
                      \r\n\
                      {\"id\":\"partial\"",
                )
                .await
                .expect("write partial response");
            tokio::time::sleep(Duration::from_secs(5)).await;
        });
        format!("http://{addr}/v1")
    }

    fn prompt() -> Prompt {
        Prompt {
            model: "m".into(),
            system: None,
            system_provider_metadata: Default::default(),
            messages: vec![Message::text(Role::User, "hi")],
            tools: vec![],
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    struct FaultFixture {
        _home: tempfile::TempDir,
        reloader: Arc<AppReloader>,
        routing_table: Arc<ConfigRoutingTable>,
        policy_table: Arc<crate::policy_table_router::PolicyTableRouter>,
        policy_runtime: Arc<crate::policy_lock::PolicyRuntime>,
        policy_store: Arc<PolicyStore>,
        initial_policy_digest: String,
        candidate_policy_digest: String,
        config_path: std::path::PathBuf,
        lock_path: std::path::PathBuf,
        access_policy_dir: std::path::PathBuf,
    }

    fn coordinated_config_yaml(model: &str, policy_table_model: &str, read_secs: u64) -> String {
        format!(
            r#"inherit_defaults: false
upstream:
  timeouts:
    read_secs: {read_secs}
providers:
  alpha:
    api_base: https://alpha.example.com/v1
    api_key: alpha-key
    api_protocol:
      - "*": chat_completions
    models:
      - {{ id: m }}
  beta:
    api_base: https://beta.example.com/v1
    api_key: beta-key
    api_protocol:
      - "*": chat_completions
    models:
      - {{ id: m }}
presets:
  coding:
    model: {model}
    policy: coding
policy_table:
  tiers:
    only: {policy_table_model}
  default_tier: only
"#
        )
    }

    fn coordinated_policy_lock_yaml(model: &str) -> String {
        format!(
            r#"lockfileVersion: 1
policies:
  coding:
    key_strategy: agent_trace
    tiers: {{ strong: {model} }}
    routes: {{}}
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong]
"#
        )
    }

    async fn fault_fixture(failure: ReloadParticipant) -> anyhow::Result<FaultFixture> {
        coordinated_fixture(Some(failure), None).await
    }

    async fn coordinated_fixture(
        failure: Option<ReloadParticipant>,
        pause: Option<Arc<TestPreparationPause>>,
    ) -> anyhow::Result<FaultFixture> {
        let home = tempfile::tempdir()?;
        let config_path = home.path().join("bitrouter.yaml");
        let lock_path = home.path().join("policy-lock.yaml");
        let access_policy_dir = home.path().join("access-policies");
        std::fs::create_dir(&access_policy_dir)?;
        std::fs::write(
            &config_path,
            coordinated_config_yaml("alpha:m", "alpha:m", 30),
        )?;
        std::fs::write(&lock_path, coordinated_policy_lock_yaml("alpha:m"))?;
        std::fs::write(
            access_policy_dir.join("operator.yaml"),
            "id: operator\nallowed_models: [alpha:m]\n",
        )?;

        // The running routing table is assembled from the same enriched shape
        // that a reload later prepares. This keeps the remote classifier from
        // treating a host-local credential provider as a candidate-only
        // startup change in this fixture.
        let mut initial = config::load(&config_path).await?;
        resolve_reloadable_config(&mut initial).await;
        let table = crate::policy_table_router::PolicyTable::from_config(&initial.policy_table)
            .ok_or_else(|| anyhow::anyhow!("initial policy table is unexpectedly inert"))?;
        let policy_table = Arc::new(crate::policy_table_router::PolicyTableRouter::new(table));
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial.clone()));
        let executor = Arc::new(HttpExecutor::new(HttpTimeouts::default())?);
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let policy_runtime = crate::policy_lock::PolicyRuntime::new(
            &initial,
            Some(&config_path),
            db,
            None,
            crate::eval::settlement::PendingEvalDecisionStore::default(),
            None,
        )
        .await?;
        let initial_policy_digest = policy_runtime
            .administration_snapshot()
            .digest
            .ok_or_else(|| anyhow::anyhow!("initial active policy has no digest"))?;
        let policy_store = Arc::new(PolicyStore::load_dir(&access_policy_dir).await?);

        // Change every participant's input before executing the test-only
        // boundary fault. The fixture therefore verifies both the participant
        // report and the live state on either side of every mutation boundary.
        std::fs::write(&config_path, coordinated_config_yaml("beta:m", "beta:m", 1))?;
        std::fs::write(&lock_path, coordinated_policy_lock_yaml("beta:m"))?;
        std::fs::write(
            access_policy_dir.join("operator.yaml"),
            "id: operator\nallowed_models: [beta:m]\n",
        )?;
        let mut candidate = config::load(&config_path).await?;
        resolve_reloadable_config(&mut candidate).await;
        let candidate_policy_digest =
            crate::policy_lock::load_for_config(&candidate, Some(&config_path))
                .await?
                .map(|loaded| loaded.digest)
                .ok_or_else(|| anyhow::anyhow!("candidate policy has no digest"))?;

        let mut reloader = AppReloader::new(
            policy_store.clone(),
            routing_table.clone(),
            executor,
            ReloadSource::File(config_path.clone()),
        )
        .with_policy_runtime(policy_runtime.clone())
        .with_policy_table_router(Some(policy_table.clone()));
        if let Some(failure) = failure {
            reloader = reloader.with_test_apply_failure(failure);
        }
        if let Some(pause) = pause {
            reloader = reloader.with_test_pause_after_prepare(pause);
        }

        Ok(FaultFixture {
            _home: home,
            reloader: Arc::new(reloader),
            routing_table,
            policy_table,
            policy_runtime,
            policy_store,
            initial_policy_digest,
            candidate_policy_digest,
            config_path,
            lock_path,
            access_policy_dir,
        })
    }

    #[tokio::test]
    async fn reload_updates_live_upstream_timeout_clients() {
        let (path, dir) = temp_config_path();
        std::fs::write(&path, config_yaml(30)).expect("write initial config");
        let initial = config::parse(&config_yaml(30)).expect("parse initial config");
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial));
        let executor = Arc::new(
            HttpExecutor::new(HttpTimeouts {
                read: Duration::from_secs(30),
                ..HttpTimeouts::default()
            })
            .expect("build executor"),
        );
        let reloader = AppReloader::new(
            Arc::new(PolicyStore::new()),
            routing_table,
            executor.clone(),
            ReloadSource::File(path.clone()),
        );

        std::fs::write(&path, config_yaml(1)).expect("write reloaded config");
        reloader.reload().await.expect("reload config");

        let api_base = stalled_json_server().await;
        let target = RoutingTarget {
            provider_name: "slow".into(),
            service_id: "m".into(),
            api_base,
            api_key: "k".into(),
            api_protocol: ApiProtocol::ChatCompletions,
            chat_token_limit_field: None,
            chat_supports_store: None,
            chat_supports_stream_options: None,
            reasoning_effort: None,
            account_label: None,
            api_key_override: None,
            api_base_override: None,
            auth_scheme: Default::default(),
        };
        let prompt = prompt();
        let ctx = PipelineContext::new(PipelineRequest::new(
            "m",
            bitrouter_sdk::caller::CallerContext::local(),
            prompt.clone(),
        ));

        let err = tokio::time::timeout(
            Duration::from_secs(3),
            executor.execute(&target, &prompt, &ctx),
        )
        .await
        .expect("reloaded read_secs should bound the stalled body")
        .expect_err("stalled body should timeout");
        std::fs::remove_dir_all(dir).ok();

        match err {
            bitrouter_sdk::BitrouterError::UpstreamTimeout => {}
            other => panic!("expected UpstreamTimeout, got {other:?}"),
        }
    }

    /// `bitrouter reload` used to return `{"status":"reloaded"}` while the
    /// daemon kept serving the tiers it started with — only `restart` applied
    /// a `policy_table:` edit. The transform is baked into the built `App` and
    /// cannot be re-registered, so nothing swapped its spec.
    #[tokio::test]
    async fn reload_rebuilds_the_policy_table_and_routes_to_the_new_provider() -> anyhow::Result<()>
    {
        use bitrouter_sdk::caller::CallerContext;
        use bitrouter_sdk::language_model::{RoutingPrefs, RoutingTable};

        let (path, dir) = temp_config_path();
        // Two providers, each serving a model of its own. The policy table's
        // one tier decides which of them a bare `m` request reaches.
        let config_with_tier = |tier_model: &str| {
            format!(
                r#"inherit_defaults: false
providers:
  alpha:
    api_base: https://alpha.example.com/v1
    api_key: k
    api_protocol:
      - "*": chat_completions
    models:
      - {{ id: m }}
  beta:
    api_base: https://beta.example.com/v1
    api_key: k
    api_protocol:
      - "*": chat_completions
    models:
      - {{ id: m }}
policy_table:
  tiers:
    only: {tier_model}
  default_tier: only
"#
            )
        };

        std::fs::write(&path, config_with_tier("alpha:m"))?;
        let initial = config::parse(&config_with_tier("alpha:m"))?;

        // The pieces the daemon holds: the routing table, and the live
        // transform built from the same config.
        let table = crate::policy_table_router::PolicyTable::from_config(&initial.policy_table)
            .ok_or_else(|| anyhow::anyhow!("the initial config defines a tier"))?;
        let router = Arc::new(crate::policy_table_router::PolicyTableRouter::new(table));
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial));
        let executor = Arc::new(HttpExecutor::new(HttpTimeouts::default())?);
        let reloader = AppReloader::new(
            Arc::new(PolicyStore::new()),
            routing_table.clone(),
            executor,
            ReloadSource::File(path.clone()),
        )
        .with_policy_table_router(Some(router.clone()));

        // Issue a request before the reload: the tier sends it to alpha.
        let provider_serving = async |model: &str| -> anyhow::Result<String> {
            let chain = routing_table
                .route_chain(model, &RoutingPrefs::default(), &CallerContext::local())
                .await?;
            chain
                .first()
                .map(|target| target.provider_name.clone())
                .ok_or_else(|| anyhow::anyhow!("no routable target for `{model}`"))
        };
        let mut before = prompt();
        router.apply(&mut before);
        assert_eq!(before.model, "alpha:m", "the starting tier");
        assert_eq!(provider_serving(&before.model).await?, "alpha");

        // Re-point the tier and reload — no restart.
        std::fs::write(&path, config_with_tier("beta:m"))?;
        reloader.reload().await?;

        // The same request must now reach the *new* provider.
        let mut after = prompt();
        router.apply(&mut after);
        std::fs::remove_dir_all(dir).ok();
        assert_eq!(
            after.model, "beta:m",
            "reload must rebuild the policy table, not keep the tiers the daemon started with"
        );
        assert_eq!(provider_serving(&after.model).await?, "beta");
        Ok(())
    }

    #[tokio::test]
    async fn invalid_policy_candidate_does_not_swap_routing_or_policy() {
        use crate::policy_lock::PolicyRuntime;
        use bitrouter_sdk::config::PolicyRuntimeMode;

        let (path, dir) = temp_config_path();
        let config_yaml = |model: &str| {
            format!(
                r#"inherit_defaults: false
presets:
  coding:
    model: {model}
    policy: coding
"#
            )
        };
        std::fs::write(&path, config_yaml("vendor:old")).expect("write initial config");
        std::fs::write(
            dir.join("policy-lock.yaml"),
            r#"lockfileVersion: 1
policies:
  coding:
    key_strategy: agent_trace
    tiers: { strong: vendor:old }
    routes: {}
    default_tier: strong
    tool_use_tier: strong
    tool_safe_tiers: [strong]
"#,
        )
        .expect("write initial policy");
        let initial = config::load(&path).await.expect("load initial config");
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial.clone()));
        let db = crate::db::connect("sqlite::memory:")
            .await
            .expect("connect db");
        crate::db::run_migrations(&db).await.expect("migrate db");
        let runtime = PolicyRuntime::new(
            &initial,
            Some(&path),
            db,
            None,
            crate::eval::settlement::PendingEvalDecisionStore::default(),
            None,
        )
        .await
        .expect("build policy runtime");
        let initial_digest = runtime
            .status(PolicyRuntimeMode::Frozen)
            .digest
            .expect("initial digest");
        let executor = Arc::new(HttpExecutor::new(HttpTimeouts::default()).expect("executor"));
        let reloader = AppReloader::new(
            Arc::new(PolicyStore::new()),
            routing_table.clone(),
            executor,
            ReloadSource::File(path.clone()),
        )
        .with_policy_runtime(runtime.clone());

        std::fs::write(&path, config_yaml("vendor:new")).expect("write candidate config");
        std::fs::write(dir.join("policy-lock.yaml"), "lockfileVersion: broken\n")
            .expect("break candidate policy");

        assert!(reloader.reload().await.is_err());
        assert_eq!(
            routing_table.snapshot_config().presets["coding"]
                .model
                .as_deref(),
            Some("vendor:old")
        );
        assert_eq!(
            runtime.status(PolicyRuntimeMode::Frozen).digest.as_deref(),
            Some(initial_digest.as_str())
        );
        std::fs::remove_dir_all(dir).ok();
    }

    #[tokio::test]
    async fn fault_boundaries_report_each_participant_and_preserve_active_policy_truth()
    -> anyhow::Result<()> {
        for failed_participant in ReloadParticipant::all() {
            let fixture = fault_fixture(failed_participant).await?;
            let initial_state = fixture
                .reloader
                .reload_state()
                .ok_or_else(|| anyhow::anyhow!("coordinator state is unavailable"))?;
            let reservation = fixture
                .reloader
                .reserve_remote(&initial_state.server_instance_id, initial_state.generation)?;
            let report = fixture.reloader.reload_reserved(reservation).await;

            assert_eq!(report.outcome, ReloadOutcome::PartiallyApplied);
            assert_eq!(
                report
                    .participants
                    .iter()
                    .filter(|entry| entry.outcome == ReloadParticipantOutcome::Applied)
                    .count(),
                4,
                "every non-failed participant should have made its prepared change"
            );
            for entry in &report.participants {
                if entry.participant == failed_participant {
                    assert_eq!(entry.outcome, ReloadParticipantOutcome::Failed);
                    assert_eq!(
                        entry.error.as_ref().map(|error| error.code.as_str()),
                        Some("fault_injected")
                    );
                } else {
                    assert_eq!(
                        entry.outcome,
                        ReloadParticipantOutcome::Applied,
                        "{failed_participant:?} must not obscure {participant:?}",
                        participant = entry.participant
                    );
                }
            }

            let state = fixture
                .reloader
                .reload_state()
                .ok_or_else(|| anyhow::anyhow!("coordinator state disappeared"))?;
            assert!(!state.running);
            assert_eq!(state.generation, 1);
            assert_eq!(state.consistency, ReloadConsistency::Mixed);
            assert_eq!(state.mixed_state_history.len(), 1);
            assert_eq!(
                state.mixed_state_history[0].outcome,
                ReloadOutcome::PartiallyApplied
            );
            assert_eq!(
                state.mixed_state_history[0].participants, report.participants,
                "the retained mixed-state record must preserve the actual participant outcomes"
            );

            let expected_routing_model = if failed_participant == ReloadParticipant::RoutingTable {
                "alpha:m"
            } else {
                "beta:m"
            };
            let active_config = fixture.routing_table.snapshot_config();
            let routed_model = active_config
                .presets
                .get("coding")
                .and_then(|preset| preset.model.as_deref())
                .ok_or_else(|| anyhow::anyhow!("coding preset is absent after reload"))?;
            assert_eq!(routed_model, expected_routing_model);

            let expected_table_model = if failed_participant == ReloadParticipant::PolicyTable {
                "alpha:m"
            } else {
                "beta:m"
            };
            let mut table_prompt = prompt();
            fixture.policy_table.apply(&mut table_prompt);
            assert_eq!(table_prompt.model, expected_table_model);

            let expected_access_model =
                if failed_participant == ReloadParticipant::AccessPolicyStore {
                    "alpha:m"
                } else {
                    "beta:m"
                };
            let access_model = fixture.policy_store.with_policy("operator", |policy| {
                policy
                    .and_then(|policy| policy.allowed_models.as_ref())
                    .and_then(|models| models.first())
                    .cloned()
            });
            assert_eq!(access_model.as_deref(), Some(expected_access_model));

            let expected_policy_digest =
                if failed_participant == ReloadParticipant::NamedPolicyRuntime {
                    fixture.initial_policy_digest.as_str()
                } else {
                    fixture.candidate_policy_digest.as_str()
                };
            assert_eq!(
                fixture
                    .policy_runtime
                    .administration_snapshot()
                    .digest
                    .as_deref(),
                Some(expected_policy_digest),
                "the active policy report must describe the policy snapshot that actually committed"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn prepared_inputs_are_not_re_read_after_source_changes() -> anyhow::Result<()> {
        let pause = Arc::new(TestPreparationPause::new());
        let fixture = coordinated_fixture(None, Some(pause.clone())).await?;
        let state = fixture
            .reloader
            .reload_state()
            .ok_or_else(|| anyhow::anyhow!("coordinator state is unavailable"))?;
        let reservation = fixture
            .reloader
            .reserve_remote(&state.server_instance_id, state.generation)?;
        let reloader = fixture.reloader.clone();
        let task = tokio::spawn(async move { reloader.reload_reserved(reservation).await });

        tokio::time::timeout(Duration::from_secs(3), pause.prepared.notified()).await?;
        // The operation has already captured beta's routing, timeout, policy
        // table, named-policy, and access-policy inputs. Revert every source
        // file before allowing mutation; any late source read would install
        // alpha instead and make this test fail.
        std::fs::write(
            &fixture.config_path,
            coordinated_config_yaml("alpha:m", "alpha:m", 30),
        )?;
        std::fs::write(&fixture.lock_path, coordinated_policy_lock_yaml("alpha:m"))?;
        std::fs::write(
            fixture.access_policy_dir.join("operator.yaml"),
            "id: operator\nallowed_models: [alpha:m]\n",
        )?;
        pause.resume.notify_one();

        let report = tokio::time::timeout(Duration::from_secs(3), task).await??;
        assert_eq!(report.outcome, ReloadOutcome::Succeeded);
        assert!(
            report
                .participants
                .iter()
                .all(|entry| entry.outcome == ReloadParticipantOutcome::Applied)
        );
        let active_config = fixture.routing_table.snapshot_config();
        let routed_model = active_config
            .presets
            .get("coding")
            .and_then(|preset| preset.model.as_deref())
            .ok_or_else(|| anyhow::anyhow!("coding preset is absent after reload"))?;
        assert_eq!(routed_model, "beta:m");
        let mut table_prompt = prompt();
        fixture.policy_table.apply(&mut table_prompt);
        assert_eq!(table_prompt.model, "beta:m");
        let access_model = fixture.policy_store.with_policy("operator", |policy| {
            policy
                .and_then(|policy| policy.allowed_models.as_ref())
                .and_then(|models| models.first())
                .cloned()
        });
        assert_eq!(access_model.as_deref(), Some("beta:m"));
        assert_eq!(
            fixture
                .policy_runtime
                .administration_snapshot()
                .digest
                .as_deref(),
            Some(fixture.candidate_policy_digest.as_str())
        );
        Ok(())
    }

    #[tokio::test]
    async fn local_and_sighup_entrypoints_cannot_overtake_remote_reload() -> anyhow::Result<()> {
        let pause = Arc::new(TestPreparationPause::new());
        let fixture = coordinated_fixture(None, Some(pause.clone())).await?;
        let state = fixture
            .reloader
            .reload_state()
            .ok_or_else(|| anyhow::anyhow!("coordinator state is unavailable"))?;
        let reservation = fixture
            .reloader
            .reserve_remote(&state.server_instance_id, state.generation)?;
        let remote_reloader = fixture.reloader.clone();
        let remote =
            tokio::spawn(async move { remote_reloader.reload_reserved(reservation).await });

        tokio::time::timeout(Duration::from_secs(3), pause.prepared.notified()).await?;
        assert!(
            fixture
                .reloader
                .reload_state()
                .is_some_and(|state| state.running)
        );

        // `DaemonCommand::Reload` calls this entry point. The coordinator must
        // reject it before `set_env_overrides` can alter process-global state.
        let override_name = format!(
            "BITROUTER_RELOAD_CONCURRENCY_{}",
            uuid::Uuid::new_v4().simple()
        );
        assert!(bitrouter_sdk::config::env_lookup(&override_name).is_none());
        let local = tokio::time::timeout(
            Duration::from_secs(1),
            fixture.reloader.reload_with_env(vec![(
                override_name.clone(),
                "must-not-be-installed".to_string(),
            )]),
        )
        .await?;
        let local_error = match local {
            Ok(()) => return Err(anyhow::anyhow!("local reload overtook remote admission")),
            Err(error) => error,
        };
        assert!(local_error.to_string().contains("reload_in_progress"));
        assert!(bitrouter_sdk::config::env_lookup(&override_name).is_none());

        // `main.rs` invokes `reload()` from SIGHUP. It must take the exact
        // same admission path rather than starting a second reload.
        let hup = tokio::time::timeout(Duration::from_secs(1), fixture.reloader.reload()).await?;
        let hup_error = match hup {
            Ok(()) => return Err(anyhow::anyhow!("SIGHUP reload overtook remote admission")),
            Err(error) => error,
        };
        assert!(hup_error.to_string().contains("reload_in_progress"));

        pause.resume.notify_one();
        let report = tokio::time::timeout(Duration::from_secs(3), remote).await??;
        assert_eq!(report.outcome, ReloadOutcome::Succeeded);
        assert_eq!(
            fixture
                .reloader
                .reload_state()
                .map(|state| state.generation),
            Some(1)
        );
        Ok(())
    }

    #[tokio::test]
    async fn remote_rejects_startup_change_applied_by_local_reload() -> anyhow::Result<()> {
        let (path, dir) = temp_config_path();
        let initial_yaml = r#"inherit_defaults: false
server:
  listen: 127.0.0.1:4356
presets:
  coding:
    model: alpha:m
"#;
        let candidate_yaml = r#"inherit_defaults: false
server:
  listen: 127.0.0.1:9999
presets:
  coding:
    model: beta:m
"#;
        std::fs::write(&path, initial_yaml)?;
        let mut initial = config::load(&path).await?;
        resolve_reloadable_config(&mut initial).await;
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial));
        let reloader = AppReloader::new(
            Arc::new(PolicyStore::new()),
            routing_table.clone(),
            Arc::new(HttpExecutor::new(HttpTimeouts::default())?),
            ReloadSource::File(path.clone()),
        );

        // Local IPC/SIGHUP retains its existing behavior: it can refresh the
        // routing snapshot even though the listener itself remains the startup
        // listener. That cannot make the field remotely reloadable later.
        std::fs::write(&path, candidate_yaml)?;
        reloader.reload().await?;
        assert_eq!(
            routing_table.snapshot_config().server.listen,
            "127.0.0.1:9999"
        );

        let state = reloader
            .reload_state()
            .ok_or_else(|| anyhow::anyhow!("coordinator state is unavailable"))?;
        assert_eq!(state.generation, 1);
        let reservation = reloader.reserve_remote(&state.server_instance_id, state.generation)?;
        let report = reloader.reload_reserved(reservation).await;

        assert_eq!(report.outcome, ReloadOutcome::Failed);
        assert!(
            report
                .restart_required_fields
                .iter()
                .any(|field| field == "server.listen"),
            "remote reload must compare startup-only fields with the daemon baseline"
        );
        assert_eq!(
            report
                .participants
                .iter()
                .find(|entry| entry.participant == ReloadParticipant::RoutingTable)
                .and_then(|entry| entry.error.as_ref())
                .map(|error| error.code.as_str()),
            Some("restart_required")
        );
        assert_eq!(
            routing_table.snapshot_config().server.listen,
            "127.0.0.1:9999"
        );
        let state = reloader
            .reload_state()
            .ok_or_else(|| anyhow::anyhow!("coordinator state disappeared"))?;
        assert_eq!(state.generation, 2);
        assert_eq!(state.consistency, ReloadConsistency::Consistent);
        let _ = std::fs::remove_dir_all(dir);
        Ok(())
    }

    #[tokio::test]
    async fn preparation_timeout_finishes_without_live_mutation() -> anyhow::Result<()> {
        let initial =
            config::parse("inherit_defaults: false\npresets:\n  coding:\n    model: alpha:m\n")?;
        let routing_table = Arc::new(ConfigRoutingTable::from_config(initial));
        let pause = Arc::new(TestPreparationPause::new());
        let reloader = Arc::new(
            AppReloader::new(
                Arc::new(PolicyStore::new()),
                routing_table.clone(),
                Arc::new(HttpExecutor::new(HttpTimeouts::default())?),
                ReloadSource::Default,
            )
            .with_test_pause_during_prepare(pause.clone())
            .with_test_preparation_timeout(Duration::from_millis(10)),
        );
        let state = reloader
            .reload_state()
            .ok_or_else(|| anyhow::anyhow!("coordinator state is unavailable"))?;
        let reservation = reloader.reserve_remote(&state.server_instance_id, state.generation)?;
        let running_reloader = reloader.clone();
        let task = tokio::spawn(async move { running_reloader.reload_reserved(reservation).await });

        tokio::time::timeout(Duration::from_secs(1), pause.prepared.notified()).await?;
        let report = tokio::time::timeout(Duration::from_secs(1), task).await??;
        assert_eq!(report.outcome, ReloadOutcome::Failed);
        assert!(report.participants.iter().all(|entry| {
            entry.participant == ReloadParticipant::RoutingTable
                || entry.outcome == ReloadParticipantOutcome::NotAttempted
        }));
        let routing_failure = report
            .participants
            .iter()
            .find(|entry| entry.participant == ReloadParticipant::RoutingTable)
            .and_then(|entry| entry.error.as_ref())
            .ok_or_else(|| anyhow::anyhow!("timeout did not identify preparation failure"))?;
        assert_eq!(routing_failure.code, "preparation_timed_out");

        let state = reloader
            .reload_state()
            .ok_or_else(|| anyhow::anyhow!("coordinator state disappeared"))?;
        assert!(!state.running);
        assert_eq!(state.generation, 1);
        assert_eq!(state.consistency, ReloadConsistency::Consistent);
        assert_eq!(
            state.last_outcome.as_ref().map(|outcome| outcome.outcome),
            Some(ReloadOutcome::Failed)
        );
        let active_config = routing_table.snapshot_config();
        assert_eq!(
            active_config
                .presets
                .get("coding")
                .and_then(|preset| preset.model.as_deref()),
            Some("alpha:m")
        );
        Ok(())
    }

    #[test]
    fn coordinator_fences_stale_and_concurrent_admission() -> Result<(), ReloadAdmissionError> {
        let coordinator = ReloadCoordinator::new();
        let initial = coordinator.state();
        let first = coordinator.reserve_remote(&initial.server_instance_id, initial.generation)?;

        assert!(matches!(
            coordinator.reserve_local(),
            Err(ReloadAdmissionError::ReloadInProgress)
        ));
        assert!(coordinator.state().running);

        // An operation-recording failure before execution drops the reservation
        // and frees admission without advancing the terminal generation.
        drop(first);
        let after_abandon = coordinator.state();
        assert!(!after_abandon.running);
        assert_eq!(after_abandon.generation, 0);

        let accepted = coordinator
            .reserve_remote(&after_abandon.server_instance_id, after_abandon.generation)?;
        let report = ReloadReport::succeeded(
            accepted.server_instance_id().to_string(),
            accepted.generation(),
        );
        coordinator.complete(&accepted, report);
        drop(accepted);

        let completed = coordinator.state();
        assert_eq!(completed.generation, 1);
        assert!(matches!(
            coordinator.reserve_remote(&completed.server_instance_id, 0),
            Err(ReloadAdmissionError::StaleGeneration)
        ));
        Ok(())
    }

    #[test]
    fn interrupted_mutation_records_unknown_mixed_state() -> anyhow::Result<()> {
        let coordinator = ReloadCoordinator::new();
        let reservation = coordinator.reserve_local()?;
        let mut progress = ReloadReport::pending(
            reservation.server_instance_id().to_string(),
            reservation.generation(),
        );
        progress.participant_applied(ReloadParticipant::RoutingTable);
        reservation.update_progress(&progress);
        reservation.mark_mutation_started();
        drop(reservation);

        let state = coordinator.state();
        let outcome = state
            .last_outcome
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("interrupted reservation did not record an outcome"))?;
        assert_eq!(state.generation, 1);
        assert!(!state.running);
        assert_eq!(state.consistency, ReloadConsistency::Mixed);
        assert_eq!(outcome.outcome, ReloadOutcome::Unknown);
        assert_eq!(
            outcome
                .participants
                .iter()
                .find(|entry| entry.participant == ReloadParticipant::RoutingTable)
                .map(|entry| entry.outcome),
            Some(ReloadParticipantOutcome::Applied),
            "interruption must retain already-recorded live changes"
        );
        assert_eq!(state.mixed_state_history.len(), 1);
        Ok(())
    }

    #[test]
    fn failed_preparation_does_not_clear_prior_mixed_state() -> anyhow::Result<()> {
        let coordinator = ReloadCoordinator::new();
        let partial_reservation = coordinator.reserve_local()?;
        let mut partial = ReloadReport::pending(
            partial_reservation.server_instance_id().to_string(),
            partial_reservation.generation(),
        );
        partial.participant_applied(ReloadParticipant::RoutingTable);
        partial.participant_failed(
            ReloadParticipant::PolicyTable,
            ReloadFailure::new("fault_injected", "policy table update failed"),
        );
        partial.finish(ReloadOutcome::PartiallyApplied);
        coordinator.complete(&partial_reservation, partial);
        drop(partial_reservation);
        assert_eq!(coordinator.state().consistency, ReloadConsistency::Mixed);

        let failed_reservation = coordinator.reserve_local()?;
        let mut failed = ReloadReport::pending(
            failed_reservation.server_instance_id().to_string(),
            failed_reservation.generation(),
        );
        failed.participant_failed(
            ReloadParticipant::RoutingTable,
            ReloadFailure::new("config_prepare_failed", "reload configuration is invalid"),
        );
        failed.finish(ReloadOutcome::Failed);
        coordinator.complete(&failed_reservation, failed);
        drop(failed_reservation);

        let state = coordinator.state();
        assert_eq!(state.generation, 2);
        assert_eq!(state.consistency, ReloadConsistency::Mixed);
        assert_eq!(state.mixed_state_history.len(), 1);
        assert_eq!(
            state.last_outcome.as_ref().map(|outcome| outcome.outcome),
            Some(ReloadOutcome::Failed)
        );
        Ok(())
    }

    #[test]
    fn remote_restart_classifier_rejects_startup_and_unknown_fields() -> anyhow::Result<()> {
        let current = config::parse("inherit_defaults: false\n")?;
        let candidate = config::parse(
            "inherit_defaults: false\nserver:\n  listen: 127.0.0.1:9999\nupstream:\n  fallback_backoff_ms: [10]\n",
        )?;
        let unclassified_fields = ["future_runtime"]
            .into_iter()
            .map(str::to_string)
            .collect::<BTreeSet<_>>();

        let fields = restart_required_fields(&current, &candidate, Some(&unclassified_fields));
        assert!(fields.contains(&"server.listen".to_string()));
        assert!(fields.contains(&"upstream.fallback_backoff_ms".to_string()));
        assert!(fields.contains(&"future_runtime".to_string()));
        Ok(())
    }

    #[test]
    fn remote_restart_classifier_allows_timeout_client_changes() -> anyhow::Result<()> {
        let current = config::parse("inherit_defaults: false\n")?;
        let candidate =
            config::parse("inherit_defaults: false\nupstream:\n  timeouts:\n    read_secs: 10\n")?;
        assert!(restart_required_fields(&current, &candidate, Some(&BTreeSet::new())).is_empty());
        Ok(())
    }

    #[test]
    fn remote_restart_classifier_rejects_unknown_nested_fields() -> anyhow::Result<()> {
        let raw = r#"inherit_defaults: false
server:
  future_field: true
upstream:
  future_field: true
providers:
  alpha:
    api_base: https://alpha.example.com/v1
    api_key: key
    future_field: true
"#;
        let current = config::parse("inherit_defaults: false\n")?;
        let candidate = config::parse(raw)?;
        let raw_candidate = serde_saphyr::from_str::<serde_json::Value>(raw)?;
        let unclassified = unclassified_config_paths(&raw_candidate);
        assert!(unclassified.contains("server.future_field"));
        assert!(unclassified.contains("upstream.future_field"));
        assert!(unclassified.contains("providers.alpha.future_field"));

        let fields = restart_required_fields(&current, &candidate, Some(&unclassified));
        assert!(fields.contains(&"server.future_field".to_string()));
        assert!(fields.contains(&"upstream.future_field".to_string()));
        assert!(fields.contains(&"providers.alpha.future_field".to_string()));
        Ok(())
    }

    #[test]
    fn remote_restart_classifier_accepts_legacy_policy_writeback_alias() -> anyhow::Result<()> {
        let current = config::parse("inherit_defaults: false\npolicy:\n  mode: frozen\n")?;
        let raw = "inherit_defaults: false\npolicy:\n  writeback: locked\n";
        let candidate = config::parse(raw)?;
        let raw_candidate = serde_saphyr::from_str::<serde_json::Value>(raw)?;
        let unclassified = unclassified_config_paths(&raw_candidate);
        assert!(!unclassified.contains("policy.writeback"));
        assert!(restart_required_fields(&current, &candidate, Some(&unclassified)).is_empty());
        Ok(())
    }

    #[test]
    fn remote_restart_classifier_rejects_inherit_defaults_change() -> anyhow::Result<()> {
        let current = config::parse("inherit_defaults: false\n")?;
        let candidate = config::parse("inherit_defaults: true\n")?;
        let fields = restart_required_fields(&current, &candidate, Some(&BTreeSet::new()));
        assert!(fields.contains(&"inherit_defaults".to_string()));
        Ok(())
    }
}
