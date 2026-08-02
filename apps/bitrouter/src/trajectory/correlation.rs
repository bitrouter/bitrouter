use anyhow::Result;
use bitrouter_sdk::language_model::protocol::responses::decode_gateway_continuation_id;
use bitrouter_sdk::language_model::{ApiProtocol, Prompt};
use sha2::{Digest, Sha256};

use super::canonical::{CanonicalPromptDigests, Canonicalizer};
use super::store::{CorrelateAndBegin, GuardedRouteInput, GuardedRouteResult, TrajectoryStore};
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
    pub ancestor_prefixes_truncated: bool,
    pub starts_with_prior_turns: bool,
    pub canonical_input_bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CorrelatedRequest {
    pub request_id: String,
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

    pub(crate) fn request_identity(
        &self,
        owner_user_id: &str,
        external_request_id: &str,
    ) -> Result<String> {
        self.canonicalizer
            .request_identity(owner_user_id, external_request_id)
    }

    pub async fn begin_request(
        &self,
        owner_user_id: &str,
        request_id: &str,
        inbound_protocol: ApiProtocol,
        prompt: &Prompt,
        captured_at: &str,
    ) -> Result<CorrelatedRequest> {
        self.begin_request_inner(
            owner_user_id,
            request_id,
            inbound_protocol,
            prompt,
            captured_at,
            None,
        )
        .await
        .map(|(correlated, _)| correlated)
    }

    pub(crate) async fn begin_guarded_request(
        &self,
        owner_user_id: &str,
        request_id: &str,
        inbound_protocol: ApiProtocol,
        prompt: &Prompt,
        captured_at: &str,
        guarded_route: GuardedRouteInput,
    ) -> Result<(CorrelatedRequest, GuardedRouteResult)> {
        let (correlated, result) = self
            .begin_request_inner(
                owner_user_id,
                request_id,
                inbound_protocol,
                prompt,
                captured_at,
                Some(guarded_route),
            )
            .await?;
        let result = result.ok_or_else(|| {
            anyhow::anyhow!("guarded trajectory request committed without a route intent")
        })?;
        Ok((correlated, result))
    }

    async fn begin_request_inner(
        &self,
        owner_user_id: &str,
        request_id: &str,
        inbound_protocol: ApiProtocol,
        prompt: &Prompt,
        captured_at: &str,
        guarded_route: Option<GuardedRouteInput>,
    ) -> Result<(CorrelatedRequest, Option<GuardedRouteResult>)> {
        let canonical = self.canonicalizer.canonicalize(prompt)?;
        let request_id = self.request_identity(owner_user_id, request_id)?;
        let external_native_parent_id = native_parent_id(&inbound_protocol, prompt)?;
        let native_parent_digest = external_native_parent_id
            .as_deref()
            .map(|parent| self.canonicalizer.native_parent_digest(parent))
            .transpose()?;
        let native_parent_id = match external_native_parent_id.as_deref() {
            Some(parent) => {
                let request_id =
                    decode_gateway_continuation_id(parent)?.unwrap_or_else(|| parent.to_owned());
                Some(self.request_identity(owner_user_id, &request_id)?)
            }
            None => None,
        };
        let evidence = correlation_evidence(native_parent_id, &canonical);
        let result = self
            .store
            .correlate_and_begin(
                owner_user_id,
                CorrelateAndBegin {
                    request_id: request_id.clone(),
                    new_episode_id: stable_id("episode", owner_user_id, &request_id),
                    event_id: stable_id("request-start", owner_user_id, &request_id),
                    correlation_key_id: self.canonicalizer.key_id().to_owned(),
                    native_parent_id: evidence.native_parent_id.clone(),
                    native_parent_digest,
                    full_input_digest: canonical.full_input_digest,
                    ancestor_prefix_digests: canonical.ancestor_prefix_digests,
                    ancestor_prefixes_truncated: canonical.ancestor_prefixes_truncated,
                    starts_with_prior_turns: canonical.starts_with_prior_turns,
                    canonical_input_bytes: canonical.canonical_input_bytes,
                    protocol: protocol_name(&inbound_protocol).to_owned(),
                    captured_at: captured_at.to_owned(),
                    guarded_route,
                },
            )
            .await?;
        Ok((
            CorrelatedRequest {
                request_id,
                episode_id: result.episode_id,
                source: result.source,
                completeness: result.completeness,
                prior_events: result.prior_events,
                evidence,
            },
            result.guarded_route,
        ))
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
        ancestor_prefixes_truncated: canonical.ancestor_prefixes_truncated,
        starts_with_prior_turns: canonical.starts_with_prior_turns,
        canonical_input_bytes: canonical.canonical_input_bytes,
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

pub(crate) fn stable_id(kind: &str, owner_user_id: &str, request_id: &str) -> String {
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
    use std::collections::BTreeMap;
    use std::sync::Arc;

    use bitrouter_sdk::language_model::{ApiProtocol, GenerationParams, Message, Prompt, Role};
    use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};

    use super::{CorrelationSource, TrajectoryRuntime};
    use crate::trajectory::canonical::{Canonicalizer, CorrelationKey};
    use crate::trajectory::store::TrajectoryStore;
    use crate::trajectory::types::{
        HistoryCompleteness, KeyedDigest, RequestStatus, Settlement, TRAJECTORY_SCHEMA_VERSION,
        TrajectoryEvent, TrajectoryEventKind, TrajectoryEvidence,
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
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([21; 32])?);
        Ok((
            TrajectoryRuntime::new(TrajectoryStore::new(db.clone()), canonicalizer),
            db,
        ))
    }

    fn runtime_with_key(
        db: &sea_orm::DatabaseConnection,
        key: CorrelationKey,
    ) -> TrajectoryRuntime {
        TrajectoryRuntime::new(TrajectoryStore::new(db.clone()), Canonicalizer::new(key))
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
        let canonicalizer = Canonicalizer::new(CorrelationKey::from_bytes([21; 32])?);
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

        let child_prompt = responses_prompt(
            vec![
                Message::text(Role::User, "prefix root"),
                Message::text(Role::Assistant, "answer"),
                Message::text(Role::User, "continue"),
            ],
            "request-native",
        );
        let linked = runtime
            .begin_request(
                "owner-a",
                "request-child",
                ApiProtocol::Responses,
                &child_prompt,
                "2026-08-01T00:00:02Z",
            )
            .await?;

        assert_eq!(linked.source, CorrelationSource::NativeParentId);
        assert_eq!(linked.episode_id, native_root.episode_id);
        assert_ne!(linked.episode_id, prefix_root.episode_id);
        assert_eq!(linked.completeness, HistoryCompleteness::Incomplete);
        assert_eq!(linked.prior_events.len(), 1);
        assert_eq!(
            linked.evidence.native_parent_id.as_deref(),
            Some(native_root.request_id.as_str())
        );
        let events = runtime
            .store()
            .events_for_episode("owner-a", &linked.episode_id)
            .await?;
        let start = events
            .last()
            .ok_or_else(|| anyhow::anyhow!("missing conflicting request start"))?;
        assert_eq!(
            start.evidence.structural.get("correlation.prefix_conflict"),
            Some(&1)
        );
        assert_eq!(
            start
                .evidence
                .categorical
                .get("history.completeness")
                .map(String::as_str),
            Some("incomplete")
        );
        assert_eq!(
            runtime
                .store()
                .episode("owner-a", &linked.episode_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("missing native episode"))?
                .completeness,
            HistoryCompleteness::Incomplete
        );

        let retry = runtime
            .begin_request(
                "owner-a",
                "request-child",
                ApiProtocol::Responses,
                &child_prompt,
                "2026-08-01T00:05:00Z",
            )
            .await?;
        assert_eq!(retry.episode_id, linked.episode_id);
        assert_eq!(retry.source, CorrelationSource::NativeParentId);
        assert_eq!(retry.completeness, HistoryCompleteness::Incomplete);
        assert_eq!(retry.prior_events, linked.prior_events);
        assert_eq!(
            runtime
                .store()
                .events_for_episode("owner-a", &linked.episode_id)
                .await?
                .len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn native_parent_matching_prefix_preserves_complete_episode() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let root = runtime
            .begin_request(
                "owner-a",
                "request-root",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "shared root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;

        let linked = runtime
            .begin_request(
                "owner-a",
                "request-matching-child",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![
                        Message::text(Role::User, "shared root"),
                        Message::text(Role::Assistant, "answer"),
                        Message::text(Role::User, "continue"),
                    ],
                    "request-root",
                ),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        assert_eq!(linked.source, CorrelationSource::NativeParentId);
        assert_eq!(linked.episode_id, root.episode_id);
        assert_eq!(linked.completeness, HistoryCompleteness::Complete);
        let events = runtime
            .store()
            .events_for_episode("owner-a", &root.episode_id)
            .await?;
        assert_eq!(
            events[1]
                .evidence
                .structural
                .get("correlation.prefix_conflict"),
            Some(&0)
        );
        assert_eq!(
            runtime
                .store()
                .episode("owner-a", &root.episode_id)
                .await?
                .ok_or_else(|| anyhow::anyhow!("missing matching episode"))?
                .completeness,
            HistoryCompleteness::Complete
        );
        Ok(())
    }

    #[tokio::test]
    async fn native_parent_outranks_ambiguous_prefix_but_marks_incomplete() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let shared = prompt(vec![Message::text(Role::User, "shared root")]);
        let native_root = runtime
            .begin_request(
                "owner-a",
                "request-native-root",
                ApiProtocol::Responses,
                &shared,
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let other_root = runtime
            .begin_request(
                "owner-a",
                "request-other-root",
                ApiProtocol::ChatCompletions,
                &shared,
                "2026-08-01T00:00:01Z",
            )
            .await?;

        let linked = runtime
            .begin_request(
                "owner-a",
                "request-ambiguous-native-child",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![
                        Message::text(Role::User, "shared root"),
                        Message::text(Role::Assistant, "answer"),
                        Message::text(Role::User, "continue"),
                    ],
                    "request-native-root",
                ),
                "2026-08-01T00:00:02Z",
            )
            .await?;

        assert_eq!(linked.source, CorrelationSource::NativeParentId);
        assert_eq!(linked.episode_id, native_root.episode_id);
        assert_ne!(linked.episode_id, other_root.episode_id);
        assert_eq!(linked.completeness, HistoryCompleteness::Incomplete);
        let events = runtime
            .store()
            .events_for_episode("owner-a", &linked.episode_id)
            .await?;
        assert_eq!(
            events[1]
                .evidence
                .structural
                .get("correlation.prefix_conflict"),
            Some(&1)
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
            .request("owner-b", &rejected.request_id)
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
        assert_eq!(
            rejected_event.evidence.structural,
            unknown_event.evidence.structural
        );
        assert_eq!(
            rejected_event.evidence.categorical,
            unknown_event.evidence.categorical
        );
        assert_ne!(
            rejected_event.evidence.digests,
            unknown_event.evidence.digests
        );
        for event in [&rejected_event, &unknown_event] {
            assert_eq!(
                event
                    .evidence
                    .structural
                    .get("correlation.native_parent_present"),
                Some(&1)
            );
            let digest = event
                .evidence
                .digests
                .get("correlation.native_parent")
                .ok_or_else(|| anyhow::anyhow!("missing native parent digest"))?;
            let digest = KeyedDigest::parse(digest.clone())?;
            assert_eq!(
                digest.key_id(),
                CorrelationKey::from_bytes([21; 32])?.key_id()
            );
        }
        let rejected_json = serde_json::to_string(&rejected_event)?;
        let unknown_json = serde_json::to_string(&unknown_event)?;
        assert!(!rejected_json.contains("request-foreign"));
        assert!(!unknown_json.contains("request-does-not-exist"));
        assert!(
            runtime
                .store()
                .request("owner-c", &unknown.request_id)
                .await?
                .is_some_and(|request| request.native_parent_id.is_none())
        );
        Ok(())
    }

    #[tokio::test]
    async fn native_parent_does_not_cross_correlation_key_epochs() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let key_a = CorrelationKey::from_bytes([21; 32])?;
        let key_b = CorrelationKey::from_bytes([22; 32])?;
        let key_b_id = key_b.key_id().to_owned();
        let runtime_a = runtime_with_key(&db, key_a);
        let runtime_b = runtime_with_key(&db, key_b);

        let root = runtime_a
            .begin_request(
                "owner-a",
                "request-key-a",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let child = runtime_b
            .begin_request(
                "owner-a",
                "request-key-b",
                ApiProtocol::Responses,
                &responses_prompt(vec![Message::text(Role::User, "child")], "request-key-a"),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        assert_eq!(child.source, CorrelationSource::Unresolved);
        assert_eq!(child.completeness, HistoryCompleteness::Incomplete);
        assert_ne!(child.episode_id, root.episode_id);
        assert!(child.prior_events.is_empty());
        assert_eq!(
            runtime_b
                .store()
                .request("owner-a", &child.request_id)
                .await?
                .and_then(|request| request.native_parent_id),
            None
        );
        let event = runtime_b
            .store()
            .events_for_episode("owner-a", &child.episode_id)
            .await?
            .remove(0);
        assert_eq!(
            event
                .evidence
                .structural
                .get("correlation.key_epoch_conflict"),
            Some(&0)
        );
        let native_digest = event
            .evidence
            .digests
            .get("correlation.native_parent")
            .ok_or_else(|| anyhow::anyhow!("missing native parent digest"))?;
        assert_eq!(
            KeyedDigest::parse(native_digest.clone())?.key_id(),
            key_b_id
        );
        Ok(())
    }

    #[tokio::test]
    async fn gateway_continuation_decodes_to_primary_request_identity() -> anyhow::Result<()> {
        let db = crate::db::connect("sqlite::memory:").await?;
        crate::db::run_migrations(&db).await?;
        let runtime_a = runtime_with_key(&db, CorrelationKey::from_bytes([21; 32])?);
        let runtime_b = runtime_with_key(&db, CorrelationKey::from_bytes([22; 32])?);
        let public_continuation_id =
            bitrouter_sdk::language_model::protocol::responses::encode_gateway_continuation_id(
                "request-root",
            )?;

        let root = runtime_a
            .begin_request(
                "owner-a",
                "request-root",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let child = runtime_a
            .begin_request(
                "owner-a",
                "request-child",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![Message::text(Role::User, "continue")],
                    &public_continuation_id,
                ),
                "2026-08-01T00:00:01Z",
            )
            .await?;
        assert_eq!(child.source, CorrelationSource::NativeParentId);
        assert_eq!(child.episode_id, root.episode_id);
        assert_eq!(child.completeness, HistoryCompleteness::Complete);
        assert_eq!(
            runtime_a
                .store()
                .request("owner-a", &child.request_id)
                .await?
                .and_then(|request| request.native_parent_id),
            Some(root.request_id.clone())
        );

        let foreign = runtime_a
            .begin_request(
                "owner-b",
                "request-foreign",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![Message::text(Role::User, "foreign")],
                    &public_continuation_id,
                ),
                "2026-08-01T00:00:02Z",
            )
            .await?;
        assert_eq!(foreign.source, CorrelationSource::Unresolved);
        assert_eq!(foreign.completeness, HistoryCompleteness::Incomplete);
        assert_ne!(foreign.episode_id, root.episode_id);

        let rotated = runtime_b
            .begin_request(
                "owner-a",
                "request-rotated",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![Message::text(Role::User, "rotated")],
                    &public_continuation_id,
                ),
                "2026-08-01T00:00:03Z",
            )
            .await?;
        assert_eq!(rotated.source, CorrelationSource::Unresolved);
        assert_eq!(rotated.completeness, HistoryCompleteness::Incomplete);
        assert_ne!(rotated.episode_id, root.episode_id);
        Ok(())
    }

    #[tokio::test]
    async fn unresolved_native_parent_retry_does_not_re_resolve_mutable_state() -> anyhow::Result<()>
    {
        let (runtime, _) = runtime().await?;
        let child_prompt = responses_prompt(
            vec![Message::text(Role::User, "child")],
            "request-late-parent",
        );
        let original = runtime
            .begin_request(
                "owner-a",
                "request-child",
                ApiProtocol::Responses,
                &child_prompt,
                "2026-08-01T00:00:00Z",
            )
            .await?;
        runtime
            .begin_request(
                "owner-a",
                "request-late-parent",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "late parent")]),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        let retry = runtime
            .begin_request(
                "owner-a",
                "request-child",
                ApiProtocol::Responses,
                &child_prompt,
                "2026-08-01T00:00:02Z",
            )
            .await?;

        assert_eq!(retry.episode_id, original.episode_id);
        assert_eq!(retry.source, CorrelationSource::Unresolved);
        assert_eq!(retry.completeness, HistoryCompleteness::Incomplete);
        assert!(retry.prior_events.is_empty());
        assert_eq!(retry.evidence, original.evidence);
        Ok(())
    }

    #[tokio::test]
    async fn trusted_native_parent_retry_remains_exact() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        runtime
            .begin_request(
                "owner-a",
                "request-parent",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "parent")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let child_prompt =
            responses_prompt(vec![Message::text(Role::User, "child")], "request-parent");
        let original = runtime
            .begin_request(
                "owner-a",
                "request-child",
                ApiProtocol::Responses,
                &child_prompt,
                "2026-08-01T00:00:01Z",
            )
            .await?;
        let retry = runtime
            .begin_request(
                "owner-a",
                "request-child",
                ApiProtocol::Responses,
                &child_prompt,
                "2026-08-01T00:00:02Z",
            )
            .await?;

        assert_eq!(retry, original);
        Ok(())
    }

    #[tokio::test]
    async fn exact_retries_distinguish_rejected_parent_values_and_absence() -> anyhow::Result<()> {
        let cases = [
            ("unknown-a", Some("missing-parent-a")),
            ("unknown-b", Some("missing-parent-b")),
            ("absent", None),
        ];
        for (initial_label, initial_parent) in cases {
            let (runtime, _) = runtime().await?;
            let request_id = format!("request-{initial_label}");
            let initial_prompt = match initial_parent {
                Some(parent) => {
                    responses_prompt(vec![Message::text(Role::User, "same input")], parent)
                }
                None => prompt(vec![Message::text(Role::User, "same input")]),
            };
            runtime
                .begin_request(
                    "owner-a",
                    &request_id,
                    ApiProtocol::Responses,
                    &initial_prompt,
                    "2026-08-01T00:00:00Z",
                )
                .await?;
            runtime
                .begin_request(
                    "owner-a",
                    &request_id,
                    ApiProtocol::Responses,
                    &initial_prompt,
                    "2026-08-01T00:01:00Z",
                )
                .await?;

            for (retry_label, retry_parent) in cases {
                if retry_label == initial_label {
                    continue;
                }
                let retry_prompt = match retry_parent {
                    Some(parent) => {
                        responses_prompt(vec![Message::text(Role::User, "same input")], parent)
                    }
                    None => prompt(vec![Message::text(Role::User, "same input")]),
                };
                let error = runtime
                    .begin_request(
                        "owner-a",
                        &request_id,
                        ApiProtocol::Responses,
                        &retry_prompt,
                        "2026-08-01T00:02:00Z",
                    )
                    .await
                    .expect_err("changed native-parent evidence must conflict");
                assert!(
                    error.to_string().contains("different content"),
                    "{initial_label} -> {retry_label}: {error}"
                );
            }
        }
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
    async fn ancestry_older_than_the_bounded_prefix_window_fails_conservatively()
    -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let root = runtime
            .begin_request(
                "owner-a",
                "request-old-root",
                ApiProtocol::Messages,
                &prompt(vec![Message::text(Role::User, "old root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let messages = (0..300)
            .map(|index| {
                if index == 0 {
                    Message::text(Role::User, "old root")
                } else if index % 2 == 0 {
                    Message::text(Role::User, format!("later user turn {index}"))
                } else {
                    Message::text(Role::Assistant, format!("later assistant turn {index}"))
                }
            })
            .collect::<Vec<_>>();

        let unresolved = runtime
            .begin_request(
                "owner-a",
                "request-bounded-history",
                ApiProtocol::Messages,
                &prompt(messages),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        assert_eq!(unresolved.source, CorrelationSource::Unresolved);
        assert_eq!(unresolved.completeness, HistoryCompleteness::Incomplete);
        assert_ne!(unresolved.episode_id, root.episode_id);
        assert!(unresolved.prior_events.is_empty());
        assert_eq!(
            unresolved.evidence.ancestor_prefix_digests.len(),
            256,
            "bounded correlation must expose only the newest authenticated prefixes"
        );
        Ok(())
    }

    #[tokio::test]
    async fn native_parent_with_omitted_older_ancestry_is_incomplete() -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let native_root = runtime
            .begin_request(
                "owner-a",
                "request-native-root",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "native root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let contradictory_root = runtime
            .begin_request(
                "owner-a",
                "request-contradictory-root",
                ApiProtocol::Messages,
                &prompt(vec![Message::text(Role::User, "omitted canonical root")]),
                "2026-08-01T00:00:01Z",
            )
            .await?;
        let messages = (0..300)
            .map(|index| {
                if index == 0 {
                    Message::text(Role::User, "omitted canonical root")
                } else if index % 2 == 0 {
                    Message::text(Role::User, format!("later user turn {index}"))
                } else {
                    Message::text(Role::Assistant, format!("later assistant turn {index}"))
                }
            })
            .collect::<Vec<_>>();
        let child_prompt = responses_prompt(messages, "request-native-root");

        let linked = runtime
            .begin_request(
                "owner-a",
                "request-truncated-native-child",
                ApiProtocol::Responses,
                &child_prompt,
                "2026-08-01T00:00:02Z",
            )
            .await?;

        assert_eq!(linked.source, CorrelationSource::NativeParentId);
        assert_eq!(linked.episode_id, native_root.episode_id);
        assert_ne!(linked.episode_id, contradictory_root.episode_id);
        assert_eq!(linked.completeness, HistoryCompleteness::Incomplete);
        assert_eq!(linked.prior_events.len(), 1);
        assert_eq!(linked.evidence.ancestor_prefix_digests.len(), 256);
        let events = runtime
            .store()
            .events_for_episode("owner-a", &linked.episode_id)
            .await?;
        let start = events
            .last()
            .ok_or_else(|| anyhow::anyhow!("missing truncated request start"))?;
        assert_eq!(
            start
                .evidence
                .structural
                .get("correlation.ancestor_prefixes_truncated"),
            Some(&1)
        );
        assert_eq!(
            start.evidence.structural.get("correlation.prefix_conflict"),
            Some(&0),
            "unobserved omitted ancestry is incomplete, not an observed contradiction"
        );

        let retry = runtime
            .begin_request(
                "owner-a",
                "request-truncated-native-child",
                ApiProtocol::Responses,
                &child_prompt,
                "2026-08-01T00:05:00Z",
            )
            .await?;
        assert_eq!(retry.episode_id, linked.episode_id);
        assert_eq!(retry.source, CorrelationSource::NativeParentId);
        assert_eq!(retry.completeness, HistoryCompleteness::Incomplete);
        assert_eq!(retry.prior_events, linked.prior_events);
        assert_eq!(
            runtime
                .store()
                .events_for_episode("owner-a", &linked.episode_id)
                .await?
                .len(),
            2
        );
        Ok(())
    }

    #[tokio::test]
    async fn exact_prefix_window_boundary_preserves_complete_native_episode() -> anyhow::Result<()>
    {
        let (runtime, _) = runtime().await?;
        let root = runtime
            .begin_request(
                "owner-a",
                "request-boundary-root",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "boundary root")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        let messages = (0..257)
            .map(|index| {
                if index == 0 {
                    Message::text(Role::User, "boundary root")
                } else if index % 2 == 0 {
                    Message::text(Role::User, format!("boundary user turn {index}"))
                } else {
                    Message::text(Role::Assistant, format!("boundary assistant turn {index}"))
                }
            })
            .collect::<Vec<_>>();

        let linked = runtime
            .begin_request(
                "owner-a",
                "request-boundary-child",
                ApiProtocol::Responses,
                &responses_prompt(messages, "request-boundary-root"),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        assert_eq!(linked.source, CorrelationSource::NativeParentId);
        assert_eq!(linked.episode_id, root.episode_id);
        assert_eq!(linked.completeness, HistoryCompleteness::Complete);
        assert_eq!(linked.evidence.ancestor_prefix_digests.len(), 256);
        let events = runtime
            .store()
            .events_for_episode("owner-a", &linked.episode_id)
            .await?;
        assert_eq!(
            events[1]
                .evidence
                .structural
                .get("correlation.ancestor_prefixes_truncated"),
            Some(&0)
        );
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
        runtime
            .store()
            .append_route_intent(
                "owner-a",
                route_intent_event(
                    &first.episode_id,
                    &first.request_id,
                    2,
                    "2026-08-01T00:00:30Z",
                )?,
            )
            .await?;
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "event-settled-retry".into(),
            owner_user_id: "owner-a".into(),
            episode_id: first.episode_id.clone(),
            request_id: Some(first.request_id.clone()),
            sequence: 3,
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
            3
        );
        Ok(())
    }

    #[tokio::test]
    async fn interleaved_settlements_preserve_latest_started_request_and_contiguous_history()
    -> anyhow::Result<()> {
        let (runtime, db) = runtime().await?;
        let root = runtime
            .begin_request(
                "owner-a",
                "request-r1",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "r1")]),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        runtime
            .begin_request(
                "owner-a",
                "request-r2",
                ApiProtocol::Responses,
                &responses_prompt(vec![Message::text(Role::User, "r2")], "request-r1"),
                "2026-08-01T00:00:01Z",
            )
            .await?;

        for (request_id, event_id, route_sequence, settle_sequence, route_at, settle_at) in [
            (
                "request-r1",
                "event-settle-r1",
                3,
                4,
                "2026-08-01T00:00:02Z",
                "2026-08-01T00:00:03Z",
            ),
            (
                "request-r2",
                "event-settle-r2",
                6,
                7,
                "2026-08-01T00:00:05Z",
                "2026-08-01T00:00:06Z",
            ),
            (
                "request-r3",
                "event-settle-r3",
                8,
                9,
                "2026-08-01T00:00:07Z",
                "2026-08-01T00:00:08Z",
            ),
        ] {
            let trajectory_request_id = runtime.request_identity("owner-a", request_id)?;
            if request_id == "request-r2" {
                runtime
                    .begin_request(
                        "owner-a",
                        "request-r3",
                        ApiProtocol::Responses,
                        &responses_prompt(vec![Message::text(Role::User, "r3")], "request-r2"),
                        "2026-08-01T00:00:04Z",
                    )
                    .await?;
            }
            runtime
                .store()
                .append_route_intent(
                    "owner-a",
                    route_intent_event(
                        &root.episode_id,
                        &trajectory_request_id,
                        route_sequence,
                        route_at,
                    )?,
                )
                .await?;
            let mut event = TrajectoryEvent {
                schema_version: TRAJECTORY_SCHEMA_VERSION,
                event_id: event_id.into(),
                owner_user_id: "owner-a".into(),
                episode_id: root.episode_id.clone(),
                request_id: Some(trajectory_request_id),
                sequence: settle_sequence,
                kind: TrajectoryEventKind::RequestSettled,
                evidence: TrajectoryEvidence {
                    structural: Default::default(),
                    categorical: Default::default(),
                    digests: Default::default(),
                },
                captured_at: settle_at.into(),
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
        }

        let events = runtime
            .store()
            .events_for_episode("owner-a", &root.episode_id)
            .await?;
        assert_eq!(
            events
                .iter()
                .map(|event| event.sequence)
                .collect::<Vec<_>>(),
            (1..=9).collect::<Vec<_>>()
        );
        let row = db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!(
                    "SELECT latest_request_id FROM trajectory_episodes WHERE episode_id = '{}'",
                    root.episode_id
                ),
            ))
            .await?
            .ok_or_else(|| anyhow::anyhow!("missing episode"))?;
        assert_eq!(
            row.try_get::<String>("", "latest_request_id")?,
            runtime.request_identity("owner-a", "request-r3")?
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
    async fn concurrent_identical_explicit_roots_reload_one_deterministic_winner()
    -> anyhow::Result<()> {
        let (runtime, db, _dir) = file_runtime().await?;
        let barrier = Arc::new(tokio::sync::Barrier::new(17));
        let tasks = (0..16)
            .map(|_| {
                let runtime = Arc::clone(&runtime);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    runtime
                        .begin_request(
                            "owner-a",
                            "request-identical-root",
                            ApiProtocol::Responses,
                            &prompt(vec![Message::text(Role::User, "same root")]),
                            "2026-08-01T00:00:00Z",
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        barrier.wait().await;
        let mut episode_id = None;
        for task in tasks {
            let result = task.await??;
            assert_eq!(
                episode_id.get_or_insert(result.episode_id.clone()),
                &result.episode_id
            );
        }

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
    async fn concurrent_conflicting_explicit_roots_keep_one_winner_and_reject_the_other()
    -> anyhow::Result<()> {
        let (runtime, db, _dir) = file_runtime().await?;
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let tasks = ["first root", "conflicting root"]
            .into_iter()
            .map(|text| {
                let runtime = Arc::clone(&runtime);
                let barrier = Arc::clone(&barrier);
                tokio::spawn(async move {
                    barrier.wait().await;
                    runtime
                        .begin_request(
                            "owner-a",
                            "request-conflicting-root",
                            ApiProtocol::Responses,
                            &prompt(vec![Message::text(Role::User, text)]),
                            "2026-08-01T00:00:00Z",
                        )
                        .await
                })
            })
            .collect::<Vec<_>>();
        barrier.wait().await;
        let results = futures::future::try_join_all(tasks).await?;
        assert_eq!(results.iter().filter(|result| result.is_ok()).count(), 1);
        let error = results
            .into_iter()
            .find_map(Result::err)
            .ok_or_else(|| anyhow::anyhow!("missing conflicting result"))?;
        assert!(error.to_string().contains("different content"));

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

    #[tokio::test]
    async fn late_native_continuation_after_retention_starts_an_incomplete_episode()
    -> anyhow::Result<()> {
        let (runtime, _) = runtime().await?;
        let root = runtime
            .begin_request(
                "owner-a",
                "request-pruned-parent",
                ApiProtocol::Responses,
                &prompt(vec![Message::text(Role::User, "old root")]),
                "2026-06-01T00:00:00Z",
            )
            .await?;
        runtime
            .store()
            .append_route_intent(
                "owner-a",
                route_intent_event(
                    &root.episode_id,
                    &root.request_id,
                    2,
                    "2026-06-01T00:00:01Z",
                )?,
            )
            .await?;
        let mut terminal = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "event-pruned-parent-settled".into(),
            owner_user_id: "owner-a".into(),
            episode_id: root.episode_id.clone(),
            request_id: Some(root.request_id.clone()),
            sequence: 3,
            kind: TrajectoryEventKind::RequestSettled,
            evidence: TrajectoryEvidence {
                structural: BTreeMap::new(),
                categorical: BTreeMap::new(),
                digests: BTreeMap::new(),
            },
            captured_at: "2026-06-01T00:00:02Z".into(),
            content_digest: String::new(),
        };
        terminal.content_digest = terminal.semantic_digest()?;
        runtime
            .store()
            .settle_request(
                "owner-a",
                Settlement {
                    event: terminal,
                    status: RequestStatus::Settled,
                    outbox: None,
                },
            )
            .await?;

        let pruned = runtime
            .store()
            .prune_before("2026-07-01T00:00:00Z", false, 10)
            .await?;
        assert_eq!(pruned.episode_rows, 1);
        assert_eq!(pruned.request_rows, 1);
        assert_eq!(pruned.event_rows, 3);
        assert!(
            runtime
                .store()
                .resolve_episode_owner(&root.episode_id)
                .await?
                .is_none()
        );

        let late = runtime
            .begin_request(
                "owner-a",
                "request-late-continuation",
                ApiProtocol::Responses,
                &responses_prompt(
                    vec![Message::text(Role::User, "continue after retention")],
                    "request-pruned-parent",
                ),
                "2026-08-01T00:00:00Z",
            )
            .await?;
        assert_ne!(late.episode_id, root.episode_id);
        assert_eq!(late.source, CorrelationSource::Unresolved);
        assert_eq!(late.completeness, HistoryCompleteness::Incomplete);
        assert!(late.prior_events.is_empty());
        assert_eq!(
            runtime
                .store()
                .request("owner-a", &late.request_id)
                .await?
                .and_then(|request| request.native_parent_id),
            None
        );
        Ok(())
    }

    fn route_intent_event(
        episode_id: &str,
        request_id: &str,
        sequence: u64,
        captured_at: &str,
    ) -> anyhow::Result<TrajectoryEvent> {
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: format!("event-route-{request_id}"),
            owner_user_id: "owner-a".into(),
            episode_id: episode_id.into(),
            request_id: Some(request_id.into()),
            sequence,
            kind: TrajectoryEventKind::RouteIntentRecorded,
            evidence: TrajectoryEvidence {
                structural: Default::default(),
                categorical: BTreeMap::from([
                    (
                        "route.projection".into(),
                        "agent_trace/v2|opening|normal".into(),
                    ),
                    ("route.selected_tier".into(), "tier-a".into()),
                    ("route.workflow_state".into(), "opening".into()),
                ]),
                digests: Default::default(),
            },
            captured_at: captured_at.into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        Ok(event)
    }
}
