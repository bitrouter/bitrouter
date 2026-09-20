//! Complete client and root-session usage aggregation for local companions.
//!
//! This read model carries opaque account references recorded from actual
//! credential selection, but no credentials or quota samples. The panel reader
//! attaches quota only to those references; client names are not account identity.

use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant};

use chrono::{DateTime, Utc};
use futures::TryStreamExt;
use sea_orm::{ColumnTrait, EntityTrait, FromQueryResult, QueryFilter, QuerySelect};
use serde::{Deserialize, Serialize};

use bitrouter_sdk::{BitrouterError, Result};

use super::MeteringStore;
use super::entities::requests;

/// Usage for every settled request in one explicit `[since, until)` window.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct CompanionUsageAggregate {
    /// Inclusive UTC lower bound.
    pub since: DateTime<Utc>,
    /// Exclusive UTC upper bound.
    pub until: DateTime<Utc>,
    /// Client groups, ordered by known tokens descending.
    pub clients: Vec<ClientUsage>,
}

/// A recognized client, or the explicit bucket for rows without client evidence.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(tag = "status", rename_all = "snake_case")]
pub enum ClientIdentity {
    /// The normalized harness persisted by session identity attribution.
    Known { harness: String },
    /// No recognized harness was persisted; no model/provider guess was made.
    Unknown,
}

/// One client total and its root-session breakdown.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ClientUsage {
    pub client: ClientIdentity,
    pub usage: UsageAggregate,
    /// Exact provider/account-ref pairs observed on settled rows. A null
    /// account ref remains unknown and must not inherit another row's account.
    pub upstream_sources: Vec<UpstreamUsageSource>,
    /// Root sessions ordered by most recent settled request first.
    pub sessions: Vec<SessionUsage>,
    /// Rows with a known client but no reliable root-session identity.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub unassigned: Option<UsageAggregate>,
    /// Most recent settled request in this client group.
    pub latest_activity_at: DateTime<Utc>,
}

/// Provider and credential-authority reference recorded at settlement.
///
/// This identifies an association for a later quota reader; it contains no
/// credential material and makes no claim that quota querying is supported.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct UpstreamUsageSource {
    pub provider_id: String,
    pub upstream_account_ref: Option<String>,
}

/// Namespace-safe identity for a root-session aggregate.
#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct SessionIdentity {
    /// Credential-derived routing namespace. This prevents equal raw session
    /// ids from different callers being merged.
    pub route_scope_id: String,
    /// Declared controller namespace, when present.
    pub controller_instance_id: Option<String>,
    /// Owning `bro launch` namespace, when one minted the request credential.
    pub launch_id: Option<String>,
    /// Which persisted identity carrier supplied the root id.
    pub source: SessionIdentitySource,
    /// Native root id, or ACP session id when no native root was observed.
    pub root_session_id: String,
}

/// Persisted carrier used as root-session evidence.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionIdentitySource {
    NativeRoot,
    AcpSession,
}

/// Usage rolled up from a root and all children sharing its root identity.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionUsage {
    pub session: SessionIdentity,
    pub usage: UsageAggregate,
    pub latest_activity_at: DateTime<Utc>,
}

/// Counts and provenance for a set of settled rows.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageAggregate {
    /// Sum of the mutually exclusive normalized buckets on known-usage rows.
    pub tokens: NormalizedTokenTotals,
    /// Sum of all five normalized buckets. Unknown requests are not zeros in
    /// this value; their presence is recorded in `provenance.unknown`.
    pub known_total_tokens: u64,
    pub request_count: u64,
    pub provenance: UsageProvenanceCounts,
}

/// Mutually exclusive token buckets persisted at settlement.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct NormalizedTokenTotals {
    pub uncached_input_tokens: u64,
    pub cache_read_tokens: u64,
    pub cache_write_tokens: u64,
    pub output_tokens: u64,
    pub reasoning_tokens: u64,
}

/// Request counts by usage provenance.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UsageProvenanceCounts {
    pub provider_reported: u64,
    pub authoritative_receipt: u64,
    pub estimated: u64,
    pub unknown: u64,
}

#[derive(Debug, Default)]
struct ClientAccumulator {
    usage: UsageAggregate,
    upstream_sources: BTreeSet<UpstreamUsageSource>,
    sessions: HashMap<SessionIdentity, SessionAccumulator>,
    unassigned: UsageAggregate,
    has_unassigned: bool,
    latest_activity_at: Option<DateTime<Utc>>,
}

#[derive(Debug)]
struct SessionAccumulator {
    usage: UsageAggregate,
    latest_activity_at: DateTime<Utc>,
}

const SNAPSHOT_TTL: Duration = Duration::from_secs(5 * 60);
const MAX_SNAPSHOTS: usize = 8;

#[derive(Debug, Default)]
pub(super) struct CompanionSnapshotCache {
    entries: Vec<CompanionSnapshot>,
}

#[derive(Debug)]
struct CompanionSnapshot {
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    inserted_at: Instant,
    aggregate: CompanionUsageAggregate,
}

impl CompanionSnapshotCache {
    fn get(
        &mut self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        now: Instant,
    ) -> Option<CompanionUsageAggregate> {
        self.remove_expired(now);
        self.entries
            .iter()
            .find(|entry| entry.since == since && entry.until == until)
            .map(|entry| entry.aggregate.clone())
    }

    fn insert_if_absent(
        &mut self,
        aggregate: CompanionUsageAggregate,
        now: Instant,
    ) -> CompanionUsageAggregate {
        self.remove_expired(now);
        if let Some(existing) = self
            .entries
            .iter()
            .find(|entry| entry.since == aggregate.since && entry.until == aggregate.until)
        {
            return existing.aggregate.clone();
        }
        if self.entries.len() >= MAX_SNAPSHOTS
            && let Some((oldest, _)) = self
                .entries
                .iter()
                .enumerate()
                .min_by_key(|(_, entry)| entry.inserted_at)
        {
            self.entries.swap_remove(oldest);
        }
        self.entries.push(CompanionSnapshot {
            since: aggregate.since,
            until: aggregate.until,
            inserted_at: now,
            aggregate: aggregate.clone(),
        });
        aggregate
    }

    fn remove_expired(&mut self, now: Instant) {
        self.entries.retain(|entry| {
            now.checked_duration_since(entry.inserted_at)
                .is_some_and(|age| age < SNAPSHOT_TTL)
        });
    }
}

/// Only the columns required by this aggregate. In particular, this excludes
/// raw usage, charge evidence, receipts, and errors from the day-wide scan.
#[derive(Debug, FromQueryResult)]
struct CompanionRow {
    provider_id: String,
    upstream_account_ref: Option<String>,
    launch_id: Option<String>,
    route_scope_id: Option<String>,
    agent_harness: Option<String>,
    controller_instance_id: Option<String>,
    acp_session_id: Option<String>,
    native_root_session_id: Option<String>,
    prompt_tokens: i64,
    completion_tokens: i64,
    uncached_input_tokens: i64,
    cache_read_tokens: i64,
    cache_write_tokens: i64,
    output_tokens: i64,
    reasoning_tokens: i64,
    usage_origin: String,
    created_at: String,
}

impl MeteringStore {
    /// Aggregate every settled request in the explicit `[since, until)` range.
    ///
    /// The query has no display-page limit. Callers may page the returned
    /// session rows later without changing the client totals.
    pub async fn aggregate_companion_usage(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
    ) -> Result<CompanionUsageAggregate> {
        if since >= until {
            return Err(BitrouterError::bad_request(
                "companion usage range requires since < until",
            ));
        }
        let mut rows = requests::Entity::find()
            .select_only()
            .column(requests::Column::ProviderId)
            .column(requests::Column::UpstreamAccountRef)
            .column(requests::Column::LaunchId)
            .column(requests::Column::RouteScopeId)
            .column(requests::Column::AgentHarness)
            .column(requests::Column::ControllerInstanceId)
            .column(requests::Column::AcpSessionId)
            .column(requests::Column::NativeRootSessionId)
            .column(requests::Column::PromptTokens)
            .column(requests::Column::CompletionTokens)
            .column(requests::Column::UncachedInputTokens)
            .column(requests::Column::CacheReadTokens)
            .column(requests::Column::CacheWriteTokens)
            .column(requests::Column::OutputTokens)
            .column(requests::Column::ReasoningTokens)
            .column(requests::Column::UsageOrigin)
            .column(requests::Column::CreatedAt)
            .filter(requests::Column::CreatedAt.gte(since.to_rfc3339()))
            .filter(requests::Column::CreatedAt.lt(until.to_rfc3339()))
            .into_model::<CompanionRow>()
            .stream(self.connection())
            .await
            .map_err(|error| {
                BitrouterError::internal(format!("aggregate_companion_usage: {error}"))
            })?;
        let mut clients = HashMap::new();
        while let Some(row) = rows.try_next().await.map_err(|error| {
            BitrouterError::internal(format!("aggregate_companion_usage: {error}"))
        })? {
            aggregate_row(&mut clients, row)?;
        }
        Ok(finish_aggregate(since, until, clients))
    }

    /// Return an immutable short-lived snapshot for stable offset pagination.
    ///
    /// Initial reads build or reuse the exact-bound snapshot. Continuations
    /// never recompute: absence or expiry is explicit so callers cannot append
    /// a page from a different ordering or total.
    pub async fn aggregate_companion_snapshot(
        &self,
        since: DateTime<Utc>,
        until: DateTime<Utc>,
        continuation: bool,
    ) -> Result<CompanionUsageAggregate> {
        let now = Instant::now();
        let cached = match self.companion_snapshots.lock() {
            Ok(mut cache) => cache.get(since, until, now),
            Err(poisoned) => poisoned.into_inner().get(since, until, now),
        };
        if let Some(cached) = cached {
            return Ok(cached);
        }
        if continuation {
            return Err(BitrouterError::bad_request("panel_snapshot_expired"));
        }

        let aggregate = self.aggregate_companion_usage(since, until).await?;
        let now = Instant::now();
        Ok(match self.companion_snapshots.lock() {
            Ok(mut cache) => cache.insert_if_absent(aggregate, now),
            Err(poisoned) => poisoned.into_inner().insert_if_absent(aggregate, now),
        })
    }
}

#[cfg(test)]
fn aggregate_rows(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    rows: Vec<CompanionRow>,
) -> Result<CompanionUsageAggregate> {
    let mut clients: HashMap<ClientIdentity, ClientAccumulator> = HashMap::new();
    for row in rows {
        aggregate_row(&mut clients, row)?;
    }
    Ok(finish_aggregate(since, until, clients))
}

fn aggregate_row(
    clients: &mut HashMap<ClientIdentity, ClientAccumulator>,
    row: CompanionRow,
) -> Result<()> {
    let activity = DateTime::parse_from_rfc3339(&row.created_at)
        .map_err(|error| {
            BitrouterError::internal(format!(
                "invalid metering created_at {:?}: {error}",
                row.created_at
            ))
        })?
        .with_timezone(&Utc);
    let client = row
        .agent_harness
        .as_ref()
        .filter(|value| !value.is_empty())
        .map(|harness| ClientIdentity::Known {
            harness: harness.clone(),
        })
        .unwrap_or(ClientIdentity::Unknown);
    let session = session_identity(&row);
    let accumulator = clients.entry(client).or_default();
    accumulator.upstream_sources.insert(UpstreamUsageSource {
        provider_id: row.provider_id.clone(),
        upstream_account_ref: row.upstream_account_ref.clone(),
    });
    accumulator.latest_activity_at = Some(
        accumulator
            .latest_activity_at
            .map_or(activity, |latest| latest.max(activity)),
    );
    accumulator.usage.add_row(&row);
    if let Some(session) = session {
        let session = accumulator
            .sessions
            .entry(session)
            .or_insert_with(|| SessionAccumulator {
                usage: UsageAggregate::default(),
                latest_activity_at: activity,
            });
        session.latest_activity_at = session.latest_activity_at.max(activity);
        session.usage.add_row(&row);
    } else {
        accumulator.has_unassigned = true;
        accumulator.unassigned.add_row(&row);
    }
    Ok(())
}

fn finish_aggregate(
    since: DateTime<Utc>,
    until: DateTime<Utc>,
    clients: HashMap<ClientIdentity, ClientAccumulator>,
) -> CompanionUsageAggregate {
    let mut clients = clients
        .into_iter()
        .filter_map(|(client, accumulator)| {
            let latest_activity_at = accumulator.latest_activity_at?;
            let mut sessions = accumulator
                .sessions
                .into_iter()
                .map(|(session, accumulator)| SessionUsage {
                    session,
                    usage: accumulator.usage,
                    latest_activity_at: accumulator.latest_activity_at,
                })
                .collect::<Vec<_>>();
            sessions.sort_by(|left, right| {
                right
                    .latest_activity_at
                    .cmp(&left.latest_activity_at)
                    .then_with(|| left.session.cmp(&right.session))
            });
            Some(ClientUsage {
                client,
                usage: accumulator.usage,
                upstream_sources: accumulator.upstream_sources.into_iter().collect(),
                sessions,
                unassigned: accumulator.has_unassigned.then_some(accumulator.unassigned),
                latest_activity_at,
            })
        })
        .collect::<Vec<_>>();
    clients.sort_by(|left, right| {
        right
            .usage
            .known_total_tokens
            .cmp(&left.usage.known_total_tokens)
            .then_with(|| right.latest_activity_at.cmp(&left.latest_activity_at))
            .then_with(|| left.client.cmp(&right.client))
    });
    CompanionUsageAggregate {
        since,
        until,
        clients,
    }
}

fn session_identity(row: &CompanionRow) -> Option<SessionIdentity> {
    let route_scope_id = row.route_scope_id.as_ref()?.clone();
    let (source, root_session_id) = if let Some(root) = row
        .native_root_session_id
        .as_ref()
        .filter(|value| !value.is_empty())
    {
        (SessionIdentitySource::NativeRoot, root.clone())
    } else {
        (
            SessionIdentitySource::AcpSession,
            row.acp_session_id
                .as_ref()
                .filter(|value| !value.is_empty())?
                .clone(),
        )
    };
    Some(SessionIdentity {
        route_scope_id,
        controller_instance_id: row.controller_instance_id.clone(),
        launch_id: row.launch_id.clone(),
        source,
        root_session_id,
    })
}

impl UsageAggregate {
    fn add_row(&mut self, row: &CompanionRow) {
        self.request_count = self.request_count.saturating_add(1);
        if !has_valid_normalized_buckets(row) {
            self.provenance.unknown = self.provenance.unknown.saturating_add(1);
            return;
        }
        match row.usage_origin.as_str() {
            "provider_reported" => {
                self.provenance.provider_reported =
                    self.provenance.provider_reported.saturating_add(1);
                self.add_known_tokens(row);
            }
            "authoritative_receipt" => {
                self.provenance.authoritative_receipt =
                    self.provenance.authoritative_receipt.saturating_add(1);
                self.add_known_tokens(row);
            }
            "estimated" => {
                self.provenance.estimated = self.provenance.estimated.saturating_add(1);
                self.add_known_tokens(row);
            }
            _ => {
                self.provenance.unknown = self.provenance.unknown.saturating_add(1);
            }
        }
    }

    fn add_known_tokens(&mut self, row: &CompanionRow) {
        self.tokens.uncached_input_tokens = self
            .tokens
            .uncached_input_tokens
            .saturating_add(non_negative(row.uncached_input_tokens));
        self.tokens.cache_read_tokens = self
            .tokens
            .cache_read_tokens
            .saturating_add(non_negative(row.cache_read_tokens));
        self.tokens.cache_write_tokens = self
            .tokens
            .cache_write_tokens
            .saturating_add(non_negative(row.cache_write_tokens));
        self.tokens.output_tokens = self
            .tokens
            .output_tokens
            .saturating_add(non_negative(row.output_tokens));
        self.tokens.reasoning_tokens = self
            .tokens
            .reasoning_tokens
            .saturating_add(non_negative(row.reasoning_tokens));
        self.known_total_tokens = self
            .known_total_tokens
            .saturating_add(non_negative(row.uncached_input_tokens))
            .saturating_add(non_negative(row.cache_read_tokens))
            .saturating_add(non_negative(row.cache_write_tokens))
            .saturating_add(non_negative(row.output_tokens))
            .saturating_add(non_negative(row.reasoning_tokens));
    }
}

fn non_negative(value: i64) -> u64 {
    value.max(0) as u64
}

fn has_valid_normalized_buckets(row: &CompanionRow) -> bool {
    let values = [
        row.prompt_tokens,
        row.completion_tokens,
        row.uncached_input_tokens,
        row.cache_read_tokens,
        row.cache_write_tokens,
        row.output_tokens,
        row.reasoning_tokens,
    ];
    values.iter().all(|value| *value >= 0)
        && row
            .uncached_input_tokens
            .checked_add(row.cache_read_tokens)
            .and_then(|sum| sum.checked_add(row.cache_write_tokens))
            == Some(row.prompt_tokens)
        && row.output_tokens.checked_add(row.reasoning_tokens) == Some(row.completion_tokens)
}

#[cfg(test)]
mod tests {
    use chrono::TimeZone;

    use super::*;

    fn row(
        created_at: &str,
        harness: Option<&str>,
        scope: Option<&str>,
        controller: Option<&str>,
        root: Option<&str>,
        origin: &str,
        tokens: i64,
    ) -> CompanionRow {
        CompanionRow {
            provider_id: "provider".to_string(),
            upstream_account_ref: None,
            launch_id: None,
            route_scope_id: scope.map(str::to_string),
            agent_harness: harness.map(str::to_string),
            controller_instance_id: controller.map(str::to_string),
            acp_session_id: None,
            native_root_session_id: root.map(str::to_string),
            prompt_tokens: tokens.saturating_mul(3),
            completion_tokens: tokens.saturating_mul(2),
            reasoning_tokens: tokens,
            cache_read_tokens: tokens,
            cache_write_tokens: tokens,
            uncached_input_tokens: tokens,
            output_tokens: tokens,
            usage_origin: origin.to_string(),
            created_at: created_at.to_string(),
        }
    }

    fn persisted_row(request_id: &str, row: CompanionRow) -> requests::Model {
        requests::Model {
            request_id: request_id.to_string(),
            user_id: "user".to_string(),
            api_key_id: "key".to_string(),
            launch_id: row.launch_id,
            route_scope_id: row.route_scope_id,
            agent_harness: row.agent_harness,
            controller_instance_id: row.controller_instance_id,
            acp_session_id: row.acp_session_id,
            native_root_session_id: row.native_root_session_id,
            native_agent_thread_id: None,
            native_parent_agent_thread_id: None,
            native_turn_id: None,
            route_lease_id: None,
            session_identity_json: None,
            router_id: None,
            binding_digest: None,
            original_selector: None,
            model_id: "model".to_string(),
            provider_id: row.provider_id,
            upstream_account_ref: row.upstream_account_ref,
            prompt_tokens: row.prompt_tokens,
            completion_tokens: row.completion_tokens,
            reasoning_tokens: row.reasoning_tokens,
            cache_read_tokens: row.cache_read_tokens,
            cache_write_tokens: row.cache_write_tokens,
            uncached_input_tokens: row.uncached_input_tokens,
            output_tokens: row.output_tokens,
            usage_origin: row.usage_origin,
            raw_usage_json: None,
            charge_status: "unknown".to_string(),
            charge_evidence_json: None,
            reconciliation_status: "not_applicable".to_string(),
            reconciliation_attempts: 0,
            reconciliation_last_error: None,
            reconciliation_last_attempt_at: None,
            authoritative_settled_at: None,
            authoritative_receipt_json: None,
            estimated_charge_micro_usd: 0,
            streamed: 0,
            latency_ms: 0,
            generation_time_ms: 0,
            error: None,
            created_at: row.created_at,
        }
    }

    fn bounds() -> Result<(DateTime<Utc>, DateTime<Utc>)> {
        let since = Utc
            .with_ymd_and_hms(2026, 9, 19, 0, 0, 0)
            .single()
            .ok_or_else(|| BitrouterError::internal("invalid test start"))?;
        let until = Utc
            .with_ymd_and_hms(2026, 9, 20, 0, 0, 0)
            .single()
            .ok_or_else(|| BitrouterError::internal("invalid test end"))?;
        Ok((since, until))
    }

    #[test]
    fn children_roll_up_to_root_without_double_counting() -> Result<()> {
        let (since, until) = bounds()?;
        let aggregate = aggregate_rows(
            since,
            until,
            vec![
                row(
                    "2026-09-19T01:00:00+00:00",
                    Some("codex"),
                    Some("local"),
                    Some("controller"),
                    Some("root-id"),
                    "provider_reported",
                    2,
                ),
                row(
                    "2026-09-19T02:00:00+00:00",
                    Some("codex"),
                    Some("local"),
                    Some("controller"),
                    Some("root-id"),
                    "estimated",
                    3,
                ),
            ],
        )?;
        assert_eq!(aggregate.clients.len(), 1);
        assert_eq!(aggregate.clients[0].sessions.len(), 1);
        assert_eq!(aggregate.clients[0].usage.request_count, 2);
        assert_eq!(aggregate.clients[0].sessions[0].usage.request_count, 2);
        assert_eq!(aggregate.clients[0].usage.known_total_tokens, 25);
        assert_eq!(aggregate.clients[0].usage.provenance.estimated, 1);
        Ok(())
    }

    #[test]
    fn equal_raw_session_ids_remain_isolated_by_namespace() -> Result<()> {
        let (since, until) = bounds()?;
        let aggregate = aggregate_rows(
            since,
            until,
            vec![
                row(
                    "2026-09-19T01:00:00Z",
                    Some("codex"),
                    Some("principal-a"),
                    Some("controller-a"),
                    Some("same"),
                    "provider_reported",
                    1,
                ),
                row(
                    "2026-09-19T02:00:00Z",
                    Some("codex"),
                    Some("principal-b"),
                    Some("controller-b"),
                    Some("same"),
                    "provider_reported",
                    1,
                ),
            ],
        )?;
        assert_eq!(aggregate.clients[0].sessions.len(), 2);
        Ok(())
    }

    #[test]
    fn equal_raw_session_ids_remain_isolated_by_launch() -> Result<()> {
        let (since, until) = bounds()?;
        let mut first = row(
            "2026-09-19T01:00:00Z",
            Some("codex"),
            Some("local"),
            None,
            Some("same"),
            "provider_reported",
            1,
        );
        first.launch_id = Some("launch-one".to_string());
        let mut second = row(
            "2026-09-19T02:00:00Z",
            Some("codex"),
            Some("local"),
            None,
            Some("same"),
            "provider_reported",
            1,
        );
        second.launch_id = Some("launch-two".to_string());
        let aggregate = aggregate_rows(since, until, vec![first, second])?;
        assert_eq!(aggregate.clients[0].sessions.len(), 2);
        Ok(())
    }

    #[test]
    fn unknown_usage_and_identity_are_explicit() -> Result<()> {
        let (since, until) = bounds()?;
        let aggregate = aggregate_rows(
            since,
            until,
            vec![row(
                "2026-09-19T01:00:00Z",
                None,
                None,
                None,
                None,
                "unknown",
                900,
            )],
        )?;
        assert_eq!(aggregate.clients[0].client, ClientIdentity::Unknown);
        assert_eq!(aggregate.clients[0].usage.known_total_tokens, 0);
        assert_eq!(aggregate.clients[0].usage.provenance.unknown, 1);
        assert_eq!(
            aggregate.clients[0]
                .unassigned
                .as_ref()
                .map(|u| u.request_count),
            Some(1)
        );
        Ok(())
    }

    #[test]
    fn inconsistent_normalized_buckets_fail_closed_to_unknown() -> Result<()> {
        let (since, until) = bounds()?;
        let mut inconsistent = row(
            "2026-09-19T01:00:00Z",
            Some("codex"),
            Some("local"),
            Some("controller"),
            Some("root"),
            "provider_reported",
            4,
        );
        inconsistent.uncached_input_tokens = 0;
        inconsistent.cache_read_tokens = 0;
        inconsistent.cache_write_tokens = 0;
        let aggregate = aggregate_rows(since, until, vec![inconsistent])?;
        assert_eq!(aggregate.clients[0].usage.known_total_tokens, 0);
        assert_eq!(aggregate.clients[0].usage.provenance.unknown, 1);
        assert_eq!(aggregate.clients[0].usage.provenance.provider_reported, 0);
        Ok(())
    }

    #[test]
    fn account_refs_are_exact_and_unknown_stays_separate() -> Result<()> {
        let (since, until) = bounds()?;
        let mut known = row(
            "2026-09-19T01:00:00Z",
            Some("codex"),
            Some("local"),
            Some("controller"),
            Some("root"),
            "provider_reported",
            1,
        );
        known.upstream_account_ref = Some("account-ref".to_string());
        let unknown = row(
            "2026-09-19T02:00:00Z",
            Some("codex"),
            Some("local"),
            Some("controller"),
            Some("root"),
            "provider_reported",
            1,
        );
        let aggregate = aggregate_rows(since, until, vec![known, unknown])?;
        assert_eq!(
            aggregate.clients[0].upstream_sources,
            vec![
                UpstreamUsageSource {
                    provider_id: "provider".to_string(),
                    upstream_account_ref: None,
                },
                UpstreamUsageSource {
                    provider_id: "provider".to_string(),
                    upstream_account_ref: Some("account-ref".to_string()),
                },
            ]
        );
        Ok(())
    }

    #[test]
    fn snapshot_cache_keeps_first_aggregate_and_expires_it() -> Result<()> {
        let (since, until) = bounds()?;
        let now = Instant::now();
        let original = aggregate_rows(
            since,
            until,
            vec![row(
                "2026-09-19T01:00:00Z",
                Some("codex"),
                Some("local"),
                None,
                Some("root"),
                "provider_reported",
                1,
            )],
        )?;
        let reconciled = aggregate_rows(
            since,
            until,
            vec![row(
                "2026-09-19T02:00:00Z",
                Some("codex"),
                Some("local"),
                None,
                Some("new-root"),
                "authoritative_receipt",
                9,
            )],
        )?;
        let mut cache = CompanionSnapshotCache::default();
        assert_eq!(cache.insert_if_absent(original.clone(), now), original);
        assert_eq!(
            cache.insert_if_absent(reconciled, now + Duration::from_secs(1)),
            original,
            "a concurrent insert or reconciliation must not replace the active snapshot"
        );
        assert!(
            cache.get(since, until, now + SNAPSHOT_TTL).is_none(),
            "a continuation must not reuse an expired snapshot"
        );
        Ok(())
    }

    #[test]
    fn snapshot_cache_is_bounded_to_eight_ranges() -> Result<()> {
        let (since, _) = bounds()?;
        let now = Instant::now();
        let mut cache = CompanionSnapshotCache::default();
        for offset in 0..=MAX_SNAPSHOTS {
            let start = since + chrono::Duration::minutes(offset as i64);
            cache.insert_if_absent(
                CompanionUsageAggregate {
                    since: start,
                    until: start + chrono::Duration::minutes(1),
                    clients: Vec::new(),
                },
                now + Duration::from_secs(offset as u64),
            );
        }
        assert_eq!(cache.entries.len(), MAX_SNAPSHOTS);
        assert!(
            !cache.entries.iter().any(|entry| entry.since == since),
            "the oldest range should be evicted"
        );
        Ok(())
    }

    #[tokio::test]
    async fn continuation_requires_a_live_snapshot() -> Result<()> {
        let (since, until) = bounds()?;
        let database = crate::db::connect("sqlite::memory:").await?;
        let store = MeteringStore::new(database);
        let missing = store
            .aggregate_companion_snapshot(since, until, true)
            .await
            .err()
            .ok_or_else(|| BitrouterError::internal("missing continuation snapshot succeeded"))?;
        assert!(missing.to_string().contains("panel_snapshot_expired"));

        let old = Instant::now()
            .checked_sub(SNAPSHOT_TTL)
            .ok_or_else(|| BitrouterError::internal("test instant underflow"))?;
        match store.companion_snapshots.lock() {
            Ok(mut cache) => {
                cache.insert_if_absent(
                    CompanionUsageAggregate {
                        since,
                        until,
                        clients: Vec::new(),
                    },
                    old,
                );
            }
            Err(poisoned) => {
                poisoned.into_inner().insert_if_absent(
                    CompanionUsageAggregate {
                        since,
                        until,
                        clients: Vec::new(),
                    },
                    old,
                );
            }
        }
        let expired = store
            .aggregate_companion_snapshot(since, until, true)
            .await
            .err()
            .ok_or_else(|| BitrouterError::internal("expired continuation snapshot succeeded"))?;
        assert!(expired.to_string().contains("panel_snapshot_expired"));
        Ok(())
    }

    #[tokio::test]
    async fn continuation_snapshot_ignores_rows_inserted_between_pages() -> Result<()> {
        let (since, until) = bounds()?;
        let database = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&database).await?;
        let store = MeteringStore::new(database);
        requests::Entity::insert(requests::ActiveModel::from(persisted_row(
            "first",
            row(
                "2026-09-19T01:00:00Z",
                Some("codex"),
                Some("local"),
                None,
                Some("root-one"),
                "provider_reported",
                1,
            ),
        )))
        .exec(store.connection())
        .await
        .map_err(|error| BitrouterError::internal(format!("insert first row: {error}")))?;
        let first = store
            .aggregate_companion_snapshot(since, until, false)
            .await?;

        requests::Entity::insert(requests::ActiveModel::from(persisted_row(
            "second",
            row(
                "2026-09-19T02:00:00Z",
                Some("codex"),
                Some("local"),
                None,
                Some("root-two"),
                "provider_reported",
                9,
            ),
        )))
        .exec(store.connection())
        .await
        .map_err(|error| BitrouterError::internal(format!("insert second row: {error}")))?;
        let continuation = store
            .aggregate_companion_snapshot(since, until, true)
            .await?;
        assert_eq!(continuation, first);
        assert_eq!(continuation.clients[0].sessions.len(), 1);
        assert_eq!(continuation.clients[0].usage.request_count, 1);
        Ok(())
    }
}
