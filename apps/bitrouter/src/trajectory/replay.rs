use std::collections::BTreeSet;

use anyhow::Result;

use super::health::reduce;
use super::store::TrajectoryStore;
use super::types::TrajectorySnapshot;

pub async fn replay_episode(
    store: &TrajectoryStore,
    owner_user_id: &str,
    episode_id: &str,
    protected_tiers: &BTreeSet<String>,
) -> Result<TrajectorySnapshot> {
    let events = store.events_for_episode(owner_user_id, episode_id).await?;
    reduce(&events, protected_tiers)
}

#[cfg(test)]
mod tests {
    use std::collections::{BTreeMap, BTreeSet};

    use bitrouter_sdk::language_model::{ApiProtocol, GenerationParams, Message, Prompt, Role};

    use super::replay_episode;
    use crate::trajectory::canonical::{Canonicalizer, CorrelationKey};
    use crate::trajectory::correlation::TrajectoryRuntime;
    use crate::trajectory::health::reduce;
    use crate::trajectory::store::TrajectoryStore;
    use crate::trajectory::types::{
        RequestStatus, Settlement, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent, TrajectoryEventKind,
        TrajectoryEvidence,
    };

    #[tokio::test]
    async fn replay_matches_direct_reduction_and_is_owner_scoped() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let runtime = TrajectoryRuntime::new(
            TrajectoryStore::new(db),
            Canonicalizer::new(CorrelationKey::from_bytes([31; 32])?),
        );
        let correlated = runtime
            .begin_request(
                "owner-a",
                "request-1",
                ApiProtocol::ChatCompletions,
                &prompt("root", None),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        runtime
            .store()
            .append_route_intent(
                "owner-a",
                intent(&correlated.episode_id, "request-1", 2, "economy", "opening")?,
            )
            .await?;
        runtime
            .store()
            .settle_request(
                "owner-a",
                settlement(&correlated.episode_id, "request-1", 3, 7, 9)?,
            )
            .await?;
        let protected_tiers = BTreeSet::from(["protected".to_owned()]);
        let events = runtime
            .store()
            .events_for_episode("owner-a", &correlated.episode_id)
            .await?;
        let direct = reduce(&events, &protected_tiers)?;

        let replayed = replay_episode(
            runtime.store(),
            "owner-a",
            &correlated.episode_id,
            &protected_tiers,
        )
        .await?;

        assert_eq!(replayed, direct);
        assert_eq!(replayed.evidence_digest, direct.evidence_digest);
        assert!(
            replay_episode(
                runtime.store(),
                "owner-b",
                &correlated.episode_id,
                &protected_tiers,
            )
            .await
            .is_err()
        );
        Ok(())
    }

    #[tokio::test]
    async fn file_sqlite_restart_replays_hold_and_next_request_prior_history() -> anyhow::Result<()>
    {
        let directory = tempfile::tempdir()?;
        let database_url = format!(
            "sqlite://{}?mode=rwc",
            directory.path().join("trajectory-restart.db").display()
        );
        let protected_tiers = BTreeSet::from(["protected".to_owned()]);
        let db = crate::db::connect(&database_url).await?;
        crate::db::run_migrations(&db).await?;
        let runtime = TrajectoryRuntime::new(
            TrajectoryStore::new(db.clone()),
            Canonicalizer::new(CorrelationKey::from_bytes([32; 32])?),
        );

        let first = runtime
            .begin_request(
                "owner-a",
                "request-1",
                ApiProtocol::ChatCompletions,
                &prompt(
                    "private-task-payload-must-not-persist",
                    Some(("x-bitrouter-benchmark-private", "private-header-marker")),
                ),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        runtime
            .store()
            .append_route_intent(
                "owner-a",
                intent(&first.episode_id, "request-1", 2, "economy", "opening")?,
            )
            .await?;
        runtime
            .store()
            .settle_request(
                "owner-a",
                settlement(&first.episode_id, "request-1", 3, 11, 3)?,
            )
            .await?;

        let second = runtime
            .begin_request(
                "owner-a",
                "request-2",
                ApiProtocol::Responses,
                &prompt(
                    "second payload",
                    Some(("previous_response_id", "request-1")),
                ),
                "2026-08-01T00:00:03Z",
            )
            .await?;
        assert_eq!(second.episode_id, first.episode_id);
        runtime
            .store()
            .append_route_intent(
                "owner-a",
                intent(&first.episode_id, "request-2", 5, "protected", "recovery")?,
            )
            .await?;
        let guard = guard(&first.episode_id, "request-2", 6, 2)?;
        runtime
            .store()
            .append_guard_activation("owner-a", guard.clone())
            .await?;
        runtime
            .store()
            .append_guard_activation("owner-a", guard)
            .await?;
        runtime
            .store()
            .settle_request(
                "owner-a",
                settlement(&first.episode_id, "request-2", 7, 13, 5)?,
            )
            .await?;

        let uninterrupted = replay_episode(
            runtime.store(),
            "owner-a",
            &first.episode_id,
            &protected_tiers,
        )
        .await?;
        assert_eq!(uninterrupted.active_hold_remaining, 2);
        let persisted = runtime
            .store()
            .events_for_episode("owner-a", &first.episode_id)
            .await?;
        assert_eq!(
            persisted[0]
                .evidence
                .structural
                .get("request.canonical_input_bytes")
                .copied(),
            Some(128)
        );
        let persisted_json = serde_json::to_string(&persisted)?;
        assert!(!persisted_json.contains("private-task-payload-must-not-persist"));
        assert!(!persisted_json.contains("private-header-marker"));
        assert!(!persisted_json.contains("x-bitrouter-benchmark-private"));

        drop(runtime);
        db.close().await?;

        let restarted_db = crate::db::connect(&database_url).await?;
        crate::db::run_migrations(&restarted_db).await?;
        let restarted_runtime = TrajectoryRuntime::new(
            TrajectoryStore::new(restarted_db),
            Canonicalizer::new(CorrelationKey::from_bytes([32; 32])?),
        );
        let restarted = replay_episode(
            restarted_runtime.store(),
            "owner-a",
            &first.episode_id,
            &protected_tiers,
        )
        .await?;
        assert_eq!(restarted, uninterrupted);
        assert_eq!(restarted.evidence_digest, uninterrupted.evidence_digest);

        let third = restarted_runtime
            .begin_request(
                "owner-a",
                "request-3",
                ApiProtocol::Responses,
                &prompt("third payload", Some(("previous_response_id", "request-2"))),
                "2026-08-01T00:00:07Z",
            )
            .await?;
        let prior_snapshot = reduce(&third.prior_events, &protected_tiers)?;
        assert_eq!(prior_snapshot, uninterrupted);
        assert_eq!(
            prior_snapshot.evidence_digest,
            uninterrupted.evidence_digest
        );
        Ok(())
    }

    fn prompt(text: &str, extra: Option<(&str, &str)>) -> Prompt {
        let mut params = GenerationParams::default();
        if let Some((key, value)) = extra {
            params
                .extra
                .insert(key.to_owned(), serde_json::json!(value));
        }
        Prompt {
            model: "inbound".to_owned(),
            system: None,
            system_provider_metadata: BTreeMap::new(),
            messages: vec![Message::text(Role::User, text)],
            tools: Vec::new(),
            params,
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn intent(
        episode_id: &str,
        request_id: &str,
        sequence: u64,
        tier: &str,
        state: &str,
    ) -> anyhow::Result<TrajectoryEvent> {
        event(
            episode_id,
            Some(request_id),
            sequence,
            TrajectoryEventKind::RouteIntentRecorded,
            BTreeMap::new(),
            BTreeMap::from([
                (
                    "route.projection".to_owned(),
                    format!("agent_trace/v2|{state}|normal"),
                ),
                ("route.selected_tier".to_owned(), tier.to_owned()),
                ("route.workflow_state".to_owned(), state.to_owned()),
            ]),
        )
    }

    fn guard(
        episode_id: &str,
        request_id: &str,
        sequence: u64,
        hold: u64,
    ) -> anyhow::Result<TrajectoryEvent> {
        event(
            episode_id,
            Some(request_id),
            sequence,
            TrajectoryEventKind::GuardActivated,
            BTreeMap::from([("guard.hold_for_requests".to_owned(), hold)]),
            BTreeMap::new(),
        )
    }

    fn settlement(
        episode_id: &str,
        request_id: &str,
        sequence: u64,
        tokens: u64,
        cost: u64,
    ) -> anyhow::Result<Settlement> {
        Ok(Settlement {
            event: event(
                episode_id,
                Some(request_id),
                sequence,
                TrajectoryEventKind::RequestSettled,
                BTreeMap::from([
                    ("settlement.total_tokens".to_owned(), tokens),
                    ("settlement.cost_micro_usd".to_owned(), cost),
                ]),
                BTreeMap::from([("settlement.outcome".to_owned(), "succeeded".to_owned())]),
            )?,
            status: RequestStatus::Settled,
            outbox: None,
        })
    }

    fn event(
        episode_id: &str,
        request_id: Option<&str>,
        sequence: u64,
        kind: TrajectoryEventKind,
        structural: BTreeMap<String, u64>,
        categorical: BTreeMap<String, String>,
    ) -> anyhow::Result<TrajectoryEvent> {
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: format!("event-{sequence}"),
            owner_user_id: "owner-a".to_owned(),
            episode_id: episode_id.to_owned(),
            request_id: request_id.map(str::to_owned),
            sequence,
            kind,
            evidence: TrajectoryEvidence {
                structural,
                categorical,
                digests: BTreeMap::new(),
            },
            captured_at: format!("2026-08-01T00:00:{:02}Z", sequence - 1),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        Ok(event)
    }
}
