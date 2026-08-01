use anyhow::Result;
use bitrouter_sdk::language_model::{ApiProtocol, Prompt};
use sha2::{Digest, Sha256};

use super::canonical::{CanonicalPromptDigests, Canonicalizer};
use super::store::{CorrelateAndBegin, TrajectoryStore};
use super::types::{HistoryCompleteness, TrajectoryEvent};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CorrelationSource {
    NativeParentId,
    CanonicalPrefix,
    ExplicitRoot,
    Unresolved,
}

impl CorrelationSource {
    pub(crate) fn as_str(self) -> &'static str {
        match self {
            Self::NativeParentId => "native_parent_id",
            Self::CanonicalPrefix => "canonical_prefix",
            Self::ExplicitRoot => "explicit_root",
            Self::Unresolved => "unresolved",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelationEvidence {
    pub native_parent_id: Option<String>,
    pub full_input_digest: String,
    pub ancestor_prefix_digests: Vec<String>,
    pub starts_with_prior_turns: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelatedRequest {
    pub episode_id: String,
    pub source: CorrelationSource,
    pub completeness: HistoryCompleteness,
    pub prior_events: Vec<TrajectoryEvent>,
    pub evidence: CorrelationEvidence,
}

pub struct TrajectoryRuntime {
    store: TrajectoryStore,
    canonicalizer: Canonicalizer,
}

#[derive(Debug, thiserror::Error)]
#[error("Responses previous_response_id must be a non-empty bounded string")]
pub(crate) struct InvalidCorrelationEvidence;

impl TrajectoryRuntime {
    pub fn new(store: TrajectoryStore, canonicalizer: Canonicalizer) -> Self {
        Self {
            store,
            canonicalizer,
        }
    }

    pub fn store(&self) -> &TrajectoryStore {
        &self.store
    }

    pub async fn begin_request(
        &self,
        owner_user_id: &str,
        request_id: &str,
        inbound_protocol: ApiProtocol,
        prompt: &Prompt,
        captured_at: &str,
    ) -> Result<CorrelatedRequest> {
        let canonical = self.canonicalizer.canonicalize(prompt)?;
        let native_parent_id = native_parent_id(&inbound_protocol, prompt)?;
        let evidence = correlation_evidence(native_parent_id, &canonical);
        let result = self
            .store
            .correlate_and_begin(
                owner_user_id,
                CorrelateAndBegin {
                    request_id: request_id.to_owned(),
                    new_episode_id: stable_id("episode", owner_user_id, request_id),
                    event_id: stable_id("request-start", owner_user_id, request_id),
                    correlation_key_id: self.canonicalizer.key_id().to_owned(),
                    native_parent_id: evidence.native_parent_id.clone(),
                    full_input_digest: canonical.full_input_digest,
                    ancestor_prefix_digests: canonical.ancestor_prefix_digests,
                    starts_with_prior_turns: canonical.starts_with_prior_turns,
                    protocol: protocol_name(&inbound_protocol).to_owned(),
                    captured_at: captured_at.to_owned(),
                },
            )
            .await?;
        Ok(CorrelatedRequest {
            episode_id: result.episode_id,
            source: result.source,
            completeness: result.completeness,
            prior_events: result.prior_events,
            evidence,
        })
    }
}

fn correlation_evidence(
    native_parent_id: Option<String>,
    canonical: &CanonicalPromptDigests,
) -> CorrelationEvidence {
    CorrelationEvidence {
        native_parent_id,
        full_input_digest: canonical.full_input_digest.as_str().to_owned(),
        ancestor_prefix_digests: canonical
            .ancestor_prefix_digests
            .iter()
            .map(|digest| digest.as_str().to_owned())
            .collect(),
        starts_with_prior_turns: canonical.starts_with_prior_turns,
    }
}

fn native_parent_id(
    protocol: &ApiProtocol,
    prompt: &Prompt,
) -> std::result::Result<Option<String>, InvalidCorrelationEvidence> {
    if protocol != &ApiProtocol::Responses {
        return Ok(None);
    }
    let Some(value) = prompt.params.extra.get("previous_response_id") else {
        return Ok(None);
    };
    let native_parent_id = value.as_str().ok_or(InvalidCorrelationEvidence)?.trim();
    if native_parent_id.is_empty()
        || native_parent_id.len() > 512
        || native_parent_id.chars().any(char::is_control)
    {
        return Err(InvalidCorrelationEvidence);
    }
    Ok(Some(native_parent_id.to_owned()))
}

fn protocol_name(protocol: &ApiProtocol) -> &str {
    match protocol {
        ApiProtocol::ChatCompletions => "chat_completions",
        ApiProtocol::Messages => "messages",
        ApiProtocol::Responses => "responses",
        ApiProtocol::GenerateContent => "generate_content",
        ApiProtocol::Custom(name) => name,
    }
}

fn stable_id(kind: &str, owner_user_id: &str, request_id: &str) -> String {
    let mut hash = Sha256::new();
    hash.update(kind.as_bytes());
    hash.update([0]);
    hash.update(owner_user_id.as_bytes());
    hash.update([0]);
    hash.update(request_id.as_bytes());
    format!("{kind}-{}", hex::encode(hash.finalize()))
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;

    use bitrouter_sdk::language_model::{ApiProtocol, GenerationParams, Message, Prompt, Role};
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use super::{CorrelationSource, TrajectoryRuntime};
    use crate::trajectory::canonical::{Canonicalizer, CorrelationKey};
    use crate::trajectory::store::TrajectoryStore;
    use crate::trajectory::types::{
        HistoryCompleteness, RequestStatus, Settlement, TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent,
        TrajectoryEventKind, TrajectoryEvidence,
    };

    fn prompt(messages: Vec<Message>) -> Prompt {
        Prompt {
            model: "inbound".into(),
            system: Some("system".into()),
            system_provider_metadata: Default::default(),
            messages,
            tools: Vec::new(),
            params: GenerationParams::default(),
            response_format: None,
            tool_choice: None,
            stream: false,
        }
    }

    fn responses_prompt(messages: Vec<Message>, previous_response_id: &str) -> Prompt {
        let mut prompt = prompt(messages);
        prompt.params.extra_protocol = Some(ApiProtocol::Responses);
        prompt.params.extra.insert(
            "previous_response_id".into(),
            serde_json::json!(previous_response_id),
        );
        prompt
    }

    async fn runtime() -> anyhow::Result<(TrajectoryRuntime, sea_orm::DatabaseConnection)> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes("install-a", [21; 32])?);
        Ok((
            TrajectoryRuntime::new(TrajectoryStore::new(db.clone()), canonicalizer),
            db,
        ))
    }

    async fn file_runtime() -> anyhow::Result<(
        Arc<TrajectoryRuntime>,
        sea_orm::DatabaseConnection,
        tempfile::TempDir,
    )> {
        let dir = tempfile::tempdir()?;
        let url = format!(
            "sqlite://{}?mode=rwc",
            dir.path().join("trajectory.db").display()
        );
        let db = crate::db::connect(&url).await?;
        crate::db::run_migrations(&db).await?;
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes("install-a", [21; 32])?);
        Ok((
            Arc::new(TrajectoryRuntime::new(
                TrajectoryStore::new(db.clone()),
                canonicalizer,
            )),
            db,
            dir,
        ))
    }

    #[tokio::test]
    async fn native_parent_resolves_exact_request_and_outranks_conflicting_prefix()
    -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let native_root = runtime
            .begin_request(
                "owner-a",
                "request-native",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "native root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let prefix_root = runtime
            .begin_request(
                "owner-a",
                "request-prefix",
                ApiProtocol::ChatCompletions,
                &prompt(vec![Message::text(Role::User, "prefix root")]),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        let linked = runtime
            .begin_request(
                "owner-a",
                "request-child",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![
                        Message::text(Role::User, "prefix root"),
                        Message::text(Role::Assistant, "answer"),
                        Message::text(Role::User, "continue"),
                    ],
                    "request-native",
                ),
                "2026-08-01T00:00:02Z",
            )
            .await?;

        assert_eq!(linked.source, CorrelationSource::NativeParentId);
        assert_eq!(linked.episode_id, native_root.episode_id);
        assert_ne!(linked.episode_id, prefix_root.episode_id);
        assert_eq!(linked.prior_events.len(), 1);
        assert_eq!(
            linked.evidence.native_parent_id.as_deref(),
            Some("request-native")
        );
        Ok(())
    }

    #[tokio::test]
    async fn cross_owner_native_parent_is_rejected_as_incomplete_without_trusted_edge()
    -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let foreign = runtime
            .begin_request(
                "owner-a",
                "request-foreign",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let rejected = runtime
            .begin_request(
                "owner-b",
                "request-local",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![Message::text(Role::User, "new owner")],
                    "request-foreign",
                ),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        assert_eq!(rejected.source, CorrelationSource::Unresolved);
        assert_eq!(rejected.completeness, HistoryCompleteness::Incomplete);
        assert_ne!(rejected.episode_id, foreign.episode_id);
        assert!(rejected.prior_events.is_empty());
        let stored = runtime
            .store()
            .request("owner-b", "request-local")
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing local request"))?;
        assert_eq!(stored.native_parent_id, None);
        let unknown = runtime
            .begin_request(
                "owner-c",
                "request-unknown-parent",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![Message::text(Role::User, "new owner")],
                    "request-does-not-exist",
                ),
                "2026-08-01T00:00:01Z",
            )
            .await?;
        assert_eq!(unknown.source, rejected.source);
        assert_eq!(unknown.completeness, rejected.completeness);
        let rejected_event = runtime
            .store()
            .events_for_episode("owner-b", &rejected.episode_id)
            .await?
            .remove(0);
        let unknown_event = runtime
            .store()
            .events_for_episode("owner-c", &unknown.episode_id)
            .await?
            .remove(0);
        assert_eq!(rejected_event.evidence, unknown_event.evidence);
        assert!(
            runtime
                .store()
                .request("owner-c", "request-unknown-parent")
                .await?
                .is_some_and(|request| request.native_parent_id.is_none())
        );
        Ok(())
    }

    #[tokio::test]
    async fn canonical_prefix_links_to_the_closest_provable_ancestor() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let root = runtime
            .begin_request(
                "owner-a",
                "request-root",
                ApiProtocol::Messages,
                &prompt(vec![Message::text(Role::User, "first")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let linked = runtime
            .begin_request(
                "owner-a",
                "request-later",
                ApiProtocol::Messages,
                &prompt(vec![
                    Message::text(Role::User, "first"),
                    Message::text(Role::Assistant, "answer"),
                    Message::text(Role::User, "next"),
                ]),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        assert_eq!(linked.source, CorrelationSource::CanonicalPrefix);
        assert_eq!(linked.episode_id, root.episode_id);
        assert_eq!(linked.completeness, HistoryCompleteness::Complete);
        assert_eq!(linked.prior_events.len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn unprovable_prior_turns_start_an_incomplete_episode() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let unresolved = runtime
            .begin_request(
                "owner-a",
                "request-unresolved",
                ApiProtocol::ChatCompletions,
                &prompt(vec![
                    Message::text(Role::User, "unknown history"),
                    Message::text(Role::Assistant, "unknown answer"),
                    Message::text(Role::User, "continue"),
                ]),
                "2026-08-01T00:00:00Z",
            )
            .await?;

        assert_eq!(unresolved.source, CorrelationSource::Unresolved);
        assert_eq!(unresolved.completeness, HistoryCompleteness::Incomplete);
        assert!(unresolved.prior_events.is_empty());
        assert!(unresolved.evidence.starts_with_prior_turns);
        Ok(())
    }

    #[tokio::test]
    async fn one_user_turn_starts_a_complete_explicit_root() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let root = runtime
            .begin_request(
                "owner-a",
                "request-root",
                ApiProtocol::ChatCompletions,
                &prompt(vec![Message::text(Role::User, "root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;

        assert_eq!(root.source, CorrelationSource::ExplicitRoot);
        assert_eq!(root.completeness, HistoryCompleteness::Complete);
        assert!(root.prior_events.is_empty());
        assert!(!root.evidence.starts_with_prior_turns);
        Ok(())
    }

    #[tokio::test]
    async fn conflicting_duplicate_request_rolls_back_without_half_written_episode()
    -> anyhow::Result<()> {
        let (runtime, db) = runtime().await?;
        let original = runtime
            .begin_request(
                "owner-a",
                "request-duplicate",
                ApiProtocol::ChatCompletions,
                &prompt(vec![Message::text(Role::User, "original")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let error = runtime
            .begin_request(
                "owner-a",
                "request-duplicate",
                ApiProtocol::ChatCompletions,
                &prompt(vec![Message::text(Role::User, "changed")]),
                "2026-08-01T00:00:01Z",
            )
            .await
            .expect_err("conflicting duplicate must fail");
        assert!(error.to_string().contains("different content"));

        assert_eq!(
            runtime
                .store()
                .events_for_episode("owner-a", &original.episode_id)
                .await?
                .len(),
            1
        );
        for table in [
            "trajectory_episodes",
            "trajectory_events",
            "trajectory_requests",
        ] {
            let row = db
                .query_one(Statement::from_string(
                    DatabaseBackend::Sqlite,
                    format!("SELECT COUNT(*) AS count FROM {table}"),
                ))
                .await?
                .ok_or_else(|| anyhow::anyhow!("missing count row"))?;
            assert_eq!(row.try_get::<i64>("", "count")?, 1, "table {table}");
        }
        Ok(())
    }

    #[tokio::test]
    async fn exact_retry_ignores_captured_at_and_returns_original_correlation() -> anyhow::Result<()>
    {
        let (runtime, _) = runtime().await?;
        let prompt = prompt(vec![Message::text(Role::User, "root")]);
        let first = runtime
            .begin_request(
                "owner-a",
                "request-retry",
                ApiProtocol::ChatCompletions,
                &prompt,
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let retry = runtime
            .begin_request(
                "owner-a",
                "request-retry",
                ApiProtocol::ChatCompletions,
                &prompt,
                "2026-08-01T00:05:00Z",
            )
            .await?;

        assert_eq!(retry.episode_id, first.episode_id);
        assert_eq!(retry.source, first.source);
        assert_eq!(retry.completeness, first.completeness);
        assert_eq!(retry.prior_events, first.prior_events);
        assert!(retry.prior_events.is_empty());
        assert_eq!(
            runtime
                .store()
                .events_for_episode("owner-a", &first.episode_id)
                .await?
                .len(),
            1
        );
        Ok(())
    }

    #[tokio::test]
    async fn exact_retry_after_settlement_returns_original_prior_events() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let prompt = prompt(vec![Message::text(Role::User, "root")]);
        let first = runtime
            .begin_request(
                "owner-a",
                "request-settled-retry",
                ApiProtocol::ChatCompletions,
                &prompt,
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "event-settled-retry".into(),
            owner_user_id: "owner-a".into(),
            episode_id: first.episode_id.clone(),
            request_id: Some("request-settled-retry".into()),
            sequence: 2,
            kind: TrajectoryEventKind::RequestSettled,
            evidence: TrajectoryEvidence {
                structural: Default::default(),
                categorical: Default::default(),
                digests: Default::default(),
            },
            captured_at: "2026-08-01T00:01:00Z".into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        runtime
            .store()
            .settle_request(
                "owner-a",
                Settlement {
                    event,
                    status: RequestStatus::Settled,
                    outbox: None,
                },
            )
            .await?;

        let retry = runtime
            .begin_request(
                "owner-a",
                "request-settled-retry",
                ApiProtocol::ChatCompletions,
                &prompt,
                "2026-08-01T00:02:00Z",
            )
            .await?;
        assert_eq!(retry.source, first.source);
        assert_eq!(retry.completeness, first.completeness);
        assert_eq!(retry.prior_events, first.prior_events);
        assert!(retry.prior_events.is_empty());
        assert_eq!(
            runtime
                .store()
                .events_for_episode("owner-a", &first.episode_id)
                .await?
                .len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn concurrent_native_children_allocate_contiguous_unique_sequences() -> anyhow::Result<()>
    {
        let (runtime, _, _dir) = file_runtime().await?;
        let root = runtime
            .begin_request(
                "owner-a",
                "request-concurrent-root",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let barrier = Arc::new(tokio::sync::Barrier::new(33));
        let tasks = (0..32)
            .map(|index| {
                let runtime = Arc::clone(&runtime);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    runtime
                        .begin_request(
                            "owner-a",
                            &format!("request-child-{index}"),
                            ApiProtocol::Responses,
                            &responses_prompt(
                                vec![Message::text(Role::User, format!("child {index}"))],
                                "request-concurrent-root",
                            ),
                            "2026-08-01T00:00:01Z",
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        barrier.wait().await;
        for task in tasks {
            task.await??;
        }

        let events = runtime
            .store()
            .events_for_episode("owner-a", &root.episode_id)
            .await?;
        assert_eq!(events.len(), 33);
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=33).collect::<Vec<_>>()
        );
        Ok(())
    }

    #[tokio::test]
    async fn corrupted_episode_heads_reject_append_without_mutation() -> anyhow::Result<()> {
        for (label, mutation) in [
            (
                "next-sequence",
                "UPDATE trajectory_episodes SET next_sequence = 99",
            ),
            (
                "latest-request",
                "UPDATE trajectory_episodes SET latest_request_id = 'wrong-request'",
            ),
            (
                "closed-state",
                "UPDATE trajectory_episodes SET closed_at = '2026-08-01T00:00:30Z'",
            ),
        ] {
            let (runtime, db) = runtime().await?;
            let root = runtime
                .begin_request(
                    "owner-a",
                    "request-root",
                    ApiProtocol::Responses,
                    &prompt(vec![Message::text(Role::User, "root")]),
                    "2026-08-01T00:00:00Z",
                )
                .await?;
            db.execute(Statement::from_string(
                DatabaseBackend::Sqlite,
                mutation.to_owned(),
            ))
            .await?;

            let error = runtime
                .begin_request(
                    "owner-a",
                    &format!("request-{label}"),
                    ApiProtocol::Responses,
                    &responses_prompt(vec![Message::text(Role::User, "child")], "request-root"),
                    "2026-08-01T00:01:00Z",
                )
                .await
                .expect_err("corrupt episode head must reject append");
            assert!(
                error.to_string().contains("episode head"),
                "{label}: {error}"
            );
            assert_eq!(
                runtime
                    .store()
                    .events_for_episode("owner-a", &root.episode_id)
                    .await?
                    .len(),
                1,
                "{label}"
            );
            assert!(
                runtime
                    .store()
                    .request("owner-a", &format!("request-{label}"))
                    .await?
                    .is_none(),
                "{label}"
            );
        }
        Ok(())
    }

    #[tokio::test]
    async fn ambiguous_longest_prefix_starts_incomplete_episode_with_generic_evidence()
    -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let shared = prompt(vec![Message::text(Role::User, "same root")]);
        let first = runtime
            .begin_request(
                "owner-a",
                "request-root-a",
                ApiProtocol::ChatCompletions,
                &shared,
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let second = runtime
            .begin_request(
                "owner-a",
                "request-root-b",
                ApiProtocol::ChatCompletions,
                &shared,
                "2026-08-01T00:00:01Z",
            )
            .await?;
        assert_ne!(first.episode_id, second.episode_id);

        let child = runtime
            .begin_request(
                "owner-a",
                "request-ambiguous-child",
                ApiProtocol::Messages,
                &prompt(vec![
                    Message::text(Role::User, "same root"),
                    Message::text(Role::Assistant, "answer"),
                    Message::text(Role::User, "continue"),
                ]),
                "2026-08-01T00:00:02Z",
            )
            .await?;

        assert_eq!(child.source, CorrelationSource::Unresolved);
        assert_eq!(child.completeness, HistoryCompleteness::Incomplete);
        assert_ne!(child.episode_id, first.episode_id);
        assert_ne!(child.episode_id, second.episode_id);
        let events = runtime
            .store()
            .events_for_episode("owner-a", &child.episode_id)
            .await?;
        assert_eq!(
            events[0]
                .evidence
                .structural
                .get("correlation.prefix_conflict"),
            Some(&1)
        );
        Ok(())
    }

    #[tokio::test]
    async fn repeated_prefix_matches_within_one_episode_remain_unambiguous() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let shared = prompt(vec![Message::text(Role::User, "same root")]);
        let root = runtime
            .begin_request(
                "owner-a",
                "request-root",
                ApiProtocol::Responses,
                &shared,
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let duplicate_digest = runtime
            .begin_request(
                "owner-a",
                "request-same-episode",
                ApiProtocol::Responses,
                &responses_prompt(vec![Message::text(Role::User, "same root")], "request-root"),
                "2026-08-01T00:00:01Z",
            )
            .await?;
        assert_eq!(duplicate_digest.episode_id, root.episode_id);

        let child = runtime
            .begin_request(
                "owner-a",
                "request-unambiguous-child",
                ApiProtocol::Messages,
                &prompt(vec![
                    Message::text(Role::User, "same root"),
                    Message::text(Role::Assistant, "answer"),
                    Message::text(Role::User, "continue"),
                ]),
                "2026-08-01T00:00:02Z",
            )
            .await?;
        assert_eq!(child.source, CorrelationSource::CanonicalPrefix);
        assert_eq!(child.episode_id, root.episode_id);
        Ok(())
    }
}
