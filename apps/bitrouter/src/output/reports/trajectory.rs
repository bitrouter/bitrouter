use std::collections::BTreeSet;

use anyhow::Result;
use serde::Serialize;

use crate::output::CliReport;
use crate::output::human::Human;
use crate::trajectory::guard::{RouteIntentClauseDisposition, validate_persisted_route_intent};
use crate::trajectory::health::reduce;
use crate::trajectory::store::PruneSummary;
use crate::trajectory::store::{EpisodeAudit, TrajectoryStore};
use crate::trajectory::types::{
    HistoryCompleteness, TrajectoryEvent, TrajectoryEventKind, TrajectorySnapshot,
};

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryInspectReport {
    pub action: String,
    pub episode_id: String,
    pub correlation_source: String,
    pub completeness: HistoryCompleteness,
    pub closed_at: Option<String>,
    pub current: TrajectorySnapshot,
    pub route_intents: Vec<TrajectoryRouteIntentReport>,
    pub events: Vec<TrajectoryEventReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryRouteIntentReport {
    pub sequence: u64,
    pub event_id: String,
    pub request_id: Option<String>,
    pub policy: Option<String>,
    pub projection: Option<String>,
    pub workflow_state: Option<String>,
    pub candidate_tier: Option<String>,
    pub selected_tier: Option<String>,
    pub selected_is_protected: Option<bool>,
    pub clauses: Vec<TrajectoryClauseReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryClauseReport {
    pub clause_id: String,
    pub disposition: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryEventReport {
    pub sequence: u64,
    pub event_id: String,
    pub request_id: Option<String>,
    pub kind: String,
    pub captured_at: String,
    pub content_digest: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryReplayReport {
    pub action: String,
    pub episode_id: String,
    pub status: String,
    pub current_digest: Option<String>,
    pub checkpoint: Option<TrajectoryDigestComparison>,
    pub corrupt_event: Option<TrajectoryCorruptEventReport>,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryDigestComparison {
    pub event_id: String,
    pub through_sequence: u64,
    pub live_digest: String,
    pub replayed_digest: String,
    pub equal: bool,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryCorruptEventReport {
    pub event_id: Option<String>,
    pub sequence: Option<u64>,
    pub reason: String,
}

#[derive(Debug, Clone, Serialize)]
pub struct TrajectoryPruneReport {
    pub action: String,
    pub before: String,
    pub dry_run: bool,
    pub delivered_outbox_rows: u64,
    pub episode_rows: u64,
    pub event_rows: u64,
    pub request_rows: u64,
}

impl TrajectoryPruneReport {
    pub fn new(before: String, dry_run: bool, summary: PruneSummary) -> Self {
        Self {
            action: "prune".to_owned(),
            before,
            dry_run,
            delivered_outbox_rows: summary.delivered_outbox_rows,
            episode_rows: summary.episode_rows,
            event_rows: summary.event_rows,
            request_rows: summary.request_rows,
        }
    }
}

pub async fn inspect_report(
    store: &TrajectoryStore,
    episode_id: &str,
) -> Result<TrajectoryInspectReport> {
    let owner = resolve_operator_episode_owner(store, episode_id).await?;
    match store.audit_episode(&owner, episode_id).await? {
        EpisodeAudit::Valid {
            episode,
            events,
            snapshot,
        } => Ok(TrajectoryInspectReport {
            action: "inspect".to_owned(),
            episode_id: episode.episode_id,
            correlation_source: episode.correlation_source,
            completeness: episode.completeness,
            closed_at: episode.closed_at,
            current: snapshot,
            route_intents: route_intents(&events)?,
            events: events.iter().map(event_report).collect(),
        }),
        EpisodeAudit::Corrupt { .. } => {
            anyhow::bail!("trajectory episode is corrupt; run trajectory replay for details")
        }
    }
}

pub async fn replay_report(
    store: &TrajectoryStore,
    episode_id: &str,
) -> Result<TrajectoryReplayReport> {
    let owner = resolve_operator_episode_owner(store, episode_id).await?;
    replay_from_audit(store.audit_episode(&owner, episode_id).await?)
}

fn replay_from_audit(audit: EpisodeAudit) -> Result<TrajectoryReplayReport> {
    match audit {
        EpisodeAudit::Valid {
            episode,
            events,
            snapshot,
        } => Ok(TrajectoryReplayReport {
            action: "replay".to_owned(),
            episode_id: episode.episode_id,
            status: "valid".to_owned(),
            current_digest: Some(snapshot.evidence_digest),
            checkpoint: latest_checkpoint(&events)?,
            corrupt_event: None,
        }),
        EpisodeAudit::Corrupt {
            episode,
            event_id,
            sequence,
            reason,
        } => Ok(TrajectoryReplayReport {
            action: "replay".to_owned(),
            episode_id: episode.episode_id,
            status: "corrupt".to_owned(),
            current_digest: None,
            checkpoint: None,
            corrupt_event: Some(TrajectoryCorruptEventReport {
                event_id,
                sequence,
                reason: stable_audit_reason(&reason).to_owned(),
            }),
        }),
    }
}

async fn resolve_operator_episode_owner(
    store: &TrajectoryStore,
    episode_id: &str,
) -> Result<String> {
    store
        .resolve_episode_owner(episode_id)
        .await?
        .ok_or_else(|| anyhow::anyhow!("trajectory episode '{episode_id}' not found"))
}

fn route_intents(events: &[TrajectoryEvent]) -> Result<Vec<TrajectoryRouteIntentReport>> {
    events
        .iter()
        .filter(|event| event.kind == TrajectoryEventKind::RouteIntentRecorded)
        .map(|event| {
            let clauses = validate_persisted_route_intent(event)?
                .into_iter()
                .flatten()
                .map(|(clause_id, disposition)| TrajectoryClauseReport {
                    clause_id,
                    disposition: disposition_name(disposition).to_owned(),
                })
                .collect();
            Ok(TrajectoryRouteIntentReport {
                sequence: event.sequence,
                event_id: event.event_id.clone(),
                request_id: event.request_id.clone(),
                policy: event.evidence.categorical.get("route.policy").cloned(),
                projection: event.evidence.categorical.get("route.projection").cloned(),
                workflow_state: event
                    .evidence
                    .categorical
                    .get("route.workflow_state")
                    .cloned(),
                candidate_tier: event
                    .evidence
                    .categorical
                    .get("route.candidate_tier")
                    .cloned(),
                selected_tier: event
                    .evidence
                    .categorical
                    .get("route.selected_tier")
                    .cloned(),
                selected_is_protected: event
                    .evidence
                    .structural
                    .get("route.selected_is_protected")
                    .map(|value| *value == 1),
                clauses,
            })
        })
        .collect()
}

fn latest_checkpoint(events: &[TrajectoryEvent]) -> Result<Option<TrajectoryDigestComparison>> {
    let Some((index, route)) = events.iter().enumerate().rev().find(|(_, event)| {
        event.kind == TrajectoryEventKind::RouteIntentRecorded
            && event.evidence.digests.contains_key("route.health_snapshot")
    }) else {
        return Ok(None);
    };
    let live_digest = route
        .evidence
        .digests
        .get("route.health_snapshot")
        .cloned()
        .ok_or_else(|| anyhow::anyhow!("trajectory checkpoint lost its live digest"))?;
    let replayed = reduce(&events[..index], &BTreeSet::new())?;
    let replayed_digest = replayed.evidence_digest;
    Ok(Some(TrajectoryDigestComparison {
        event_id: route.event_id.clone(),
        through_sequence: route
            .sequence
            .checked_sub(1)
            .ok_or_else(|| anyhow::anyhow!("trajectory checkpoint has invalid sequence"))?,
        equal: live_digest == replayed_digest,
        live_digest,
        replayed_digest,
    }))
}

fn event_report(event: &TrajectoryEvent) -> TrajectoryEventReport {
    TrajectoryEventReport {
        sequence: event.sequence,
        event_id: event.event_id.clone(),
        request_id: event.request_id.clone(),
        kind: event_kind_name(event.kind).to_owned(),
        captured_at: event.captured_at.clone(),
        content_digest: event.content_digest.clone(),
    }
}

fn event_kind_name(kind: TrajectoryEventKind) -> &'static str {
    match kind {
        TrajectoryEventKind::RequestStarted => "request_started",
        TrajectoryEventKind::RouteIntentRecorded => "route_intent_recorded",
        TrajectoryEventKind::RequestSettled => "request_settled",
        TrajectoryEventKind::GuardActivated => "guard_activated",
        TrajectoryEventKind::EpisodeClosed => "episode_closed",
    }
}

fn disposition_name(disposition: RouteIntentClauseDisposition) -> &'static str {
    match disposition {
        RouteIntentClauseDisposition::Applied => "applied",
        RouteIntentClauseDisposition::Skipped => "skipped",
    }
}

fn stable_audit_reason(reason: &str) -> &'static str {
    match reason {
        "stored_event_invalid" => "stored_event_invalid",
        "sequence_gap" => "sequence_gap",
        "reducer_rejected_prefix" => "reducer_rejected_prefix",
        "episode_head_mismatch" => "episode_head_mismatch",
        _ => "audit_rejected",
    }
}

impl CliReport for TrajectoryInspectReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line(&format!("trajectory {}", self.episode_id))?;
        human.line(&format!(
            "correlation {} / {:?}",
            self.correlation_source, self.completeness
        ))?;
        human.line(&format!(
            "health {} (sequence {}, active hold {})",
            self.current.evidence_digest,
            self.current.through_sequence,
            self.current.active_hold_remaining
        ))?;
        for route in &self.route_intents {
            human.line(&format!(
                "route {} {} -> {}",
                display_optional(route.policy.as_deref(), "unbound"),
                display_optional(route.candidate_tier.as_deref(), "unknown"),
                display_optional(route.selected_tier.as_deref(), "unknown")
            ))?;
        }
        for event in &self.events {
            human.line(&format!(
                "event {} {} {}",
                event.sequence, event.kind, event.content_digest
            ))?;
        }
        Ok(())
    }
}

impl CliReport for TrajectoryReplayReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line(&format!(
            "trajectory replay {}: {}",
            self.episode_id, self.status
        ))?;
        if let Some(checkpoint) = &self.checkpoint {
            human.line(&format!(
                "checkpoint {} live={} replayed={} equal={}",
                checkpoint.event_id,
                checkpoint.live_digest,
                checkpoint.replayed_digest,
                checkpoint.equal
            ))?;
        }
        if let Some(corrupt) = &self.corrupt_event {
            let sequence = corrupt.sequence.map(|value| value.to_string());
            human.line(&format!(
                "first corrupt event {} sequence {}: {}",
                display_optional(corrupt.event_id.as_deref(), "unknown"),
                display_optional(sequence.as_deref(), "unknown"),
                corrupt.reason
            ))?;
        }
        Ok(())
    }
}

fn display_optional<'a>(value: Option<&'a str>, fallback: &'a str) -> &'a str {
    match value {
        Some(value) => value,
        None => fallback,
    }
}

impl CliReport for TrajectoryPruneReport {
    fn render(&self, human: &mut Human<'_>) -> std::io::Result<()> {
        human.line(&format!(
            "trajectory prune before {}{}",
            self.before,
            if self.dry_run { " (dry run)" } else { "" }
        ))?;
        human.line(&format!(
            "delivered outbox {}, episodes {}, events {}, requests {}",
            self.delivered_outbox_rows, self.episode_rows, self.event_rows, self.request_rows
        ))
    }
}

#[cfg(test)]
mod tests {
    use std::collections::BTreeSet;

    use super::{inspect_report, replay_from_audit, replay_report};
    use crate::output::{Format, Output};
    use crate::trajectory::guard::{IncompleteHistoryAction, ProgressGuardPolicy};
    use crate::trajectory::store::{
        CorrelateAndBegin, EpisodeAudit, GuardedRouteInput, TrajectoryStore,
    };
    use crate::trajectory::types::{HistoryCompleteness, StoredEpisode};
    use crate::workflow_state::ir::RouteProjection;

    const POLICY_DIGEST: &str =
        "sha256:0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";

    #[tokio::test]
    async fn inspect_and_replay_reports_are_stable_structural_and_digest_equal()
    -> anyhow::Result<()> {
        let store = guarded_store().await?;
        let inspect = inspect_report(&store, "episode-report").await?;
        let value = serde_json::to_value(&inspect)?;
        assert_eq!(value["action"], "inspect");
        assert_eq!(value["episode_id"], "episode-report");
        assert_eq!(value["correlation_source"], "explicit_root");
        assert_eq!(value["completeness"], "complete");
        assert_eq!(value["current"]["through_sequence"], 2);
        assert_eq!(value["current"]["active_hold_remaining"], 0);
        assert_eq!(value["route_intents"][0]["policy"], "coding");
        assert_eq!(value["route_intents"][0]["candidate_tier"], "economy");
        assert_eq!(value["route_intents"][0]["selected_tier"], "economy");
        assert_eq!(
            value["route_intents"][0]["clauses"]
                .as_array()
                .map(Vec::len),
            Some(9)
        );
        assert_eq!(
            value["events"]
                .as_array()
                .and_then(|events| events.get(1))
                .and_then(|event| event.get("content_digest")),
            Some(&serde_json::Value::String(
                inspect.events[1].content_digest.clone()
            ))
        );

        let replay = replay_report(&store, "episode-report").await?;
        let replay_value = serde_json::to_value(&replay)?;
        assert_eq!(replay_value["action"], "replay");
        assert_eq!(replay_value["status"], "valid");
        assert_eq!(replay_value["checkpoint"]["equal"], true);
        assert_eq!(
            replay_value["checkpoint"]["live_digest"],
            replay_value["checkpoint"]["replayed_digest"]
        );
        assert!(replay_value["corrupt_event"].is_null());

        let json = String::from_utf8(Output::new(Format::Json).render_to_vec(&inspect))?;
        let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&inspect))?;
        assert!(json.contains("\"correlation_source\": \"explicit_root\""));
        assert!(human.contains("trajectory episode-report"));
        assert!(human.contains("route coding"));
        Ok(())
    }

    #[tokio::test]
    async fn corrupt_replay_reports_only_a_stable_reason_code() -> anyhow::Result<()> {
        let sentinel = "private-metadata-SUPER-SECRET-sentinel";
        let replay = replay_from_audit(EpisodeAudit::Corrupt {
            episode: StoredEpisode {
                episode_id: "episode-report".into(),
                owner_user_id: "operator-owner".into(),
                correlation_source: "explicit_root".into(),
                correlation_key_id: "key-report".into(),
                completeness: HistoryCompleteness::Complete,
                next_sequence: 3,
                first_captured_at: "2026-08-01T00:00:00Z".into(),
                last_captured_at: "2026-08-01T00:00:00Z".into(),
                closed_at: None,
                latest_request_id: Some("request-report".into()),
            },
            event_id: Some("route-report".into()),
            sequence: Some(2),
            reason: sentinel.into(),
        })?;
        let json = String::from_utf8(Output::new(Format::Json).render_to_vec(&replay))?;
        let human = String::from_utf8(Output::new(Format::Human).render_to_vec(&replay))?;
        assert_eq!(replay.status, "corrupt");
        assert_eq!(
            replay
                .corrupt_event
                .as_ref()
                .map(|event| event.reason.as_str()),
            Some("audit_rejected")
        );
        assert!(!json.contains(sentinel));
        assert!(!human.contains(sentinel));
        Ok(())
    }

    async fn guarded_store() -> anyhow::Result<TrajectoryStore> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let store = TrajectoryStore::new(db);
        store
            .correlate_and_begin(
                "operator-owner",
                CorrelateAndBegin {
                    request_id: "request-report".into(),
                    new_episode_id: "episode-report".into(),
                    event_id: "start-report".into(),
                    correlation_key_id: "key-report".into(),
                    native_parent_id: None,
                    native_parent_digest: None,
                    full_input_digest: crate::trajectory::types::KeyedDigest::parse(format!(
                        "hmac-sha256:key-report:{}",
                        "1".repeat(64)
                    ))?,
                    ancestor_prefix_digests: Vec::new(),
                    ancestor_prefixes_truncated: false,
                    starts_with_prior_turns: false,
                    canonical_input_bytes: 64,
                    protocol: "chat_completions".into(),
                    captured_at: "2026-08-01T00:00:00Z".into(),
                    guarded_route: Some(GuardedRouteInput {
                        route_event_id: "route-report".into(),
                        guard_event_id: "guard-report".into(),
                        policy_name: "coding".into(),
                        request_key: "agent_trace/v2|opening|normal".into(),
                        baseline_tier: Some("strong".into()),
                        baseline_effort: None,
                        tier_efforts: Default::default(),
                        preset: Some("coding".into()),
                        projection: RouteProjection::parse_key("agent_trace/v2|opening|normal")
                            .ok_or_else(|| anyhow::anyhow!("invalid fixture projection"))?,
                        candidate_tier: Some("economy".into()),
                        policy_digest: POLICY_DIGEST.into(),
                        policy: ProgressGuardPolicy {
                            escalation_tier: "strong".into(),
                            protected_tiers: BTreeSet::from(["strong".into()]),
                            max_consecutive_unprotected: Some(3),
                            max_same_projection_unprotected: Some(3),
                            max_recovery_count: Some(2),
                            max_episode_requests: Some(4),
                            max_episode_elapsed_ms: None,
                            max_episode_cost_micro_usd: None,
                            hold_for_requests: 2,
                            incomplete_history: IncompleteHistoryAction::Observe,
                        },
                        carries_tools: false,
                        tool_use_tier: Some("strong".into()),
                        tool_safe_tiers: BTreeSet::from(["strong".into()]),
                    }),
                },
            )
            .await?;
        Ok(store)
    }
}
