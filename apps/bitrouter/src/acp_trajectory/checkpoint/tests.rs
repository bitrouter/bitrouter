use bitrouter_sdk::acp::capture::CapturePort;
use serde_json::json;

use super::*;
use crate::acp_trajectory::tests::{event, identity, new_session, scope, seed_request, store};

async fn update(recorder: &dyn CapturePort, session: &str, text: &str) -> Result<()> {
    recorder.record(event(CaptureKind::Notification, None, "session/update", json!({"sessionId":session,"update":{"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":text}}}))).await?;
    Ok(())
}

async fn freeze(store: &CanonicalStore, id: &str) -> Result<Checkpoint> {
    let identity = identity(id);
    let head = session(&store.db, &identity.key()?).await?.head;
    store.freeze_checkpoint(&identity, head).await
}

fn input(
    cp: &Checkpoint,
    id: &str,
    expected: Option<&str>,
    source: AssessmentSource,
) -> RevisionInput {
    RevisionInput {
        submission_id: id.into(),
        checkpoint_id: cp.checkpoint_id.clone(),
        expected_revision: expected.map(str::to_owned),
        source,
        evaluator_id: "fixture-evaluator".into(),
        evaluator_version: "1".into(),
        reason: "recorded evidence".into(),
        assessment: Some(AssessmentContent {
            pipeline_config_digest: "a".repeat(64),
            selection_digest: "b".repeat(64),
            scores: BTreeMap::from([
                (
                    "correctness".into(),
                    CriterionScore::Scored { value_ppm: 500_000 },
                ),
                ("delivery".into(), CriterionScore::Unknown),
                ("pr".into(), CriterionScore::NotApplicable),
            ]),
            evidence: cp
                .segments
                .iter()
                .flat_map(|s| &s.events)
                .take(1)
                .map(|r| EvidenceCitation {
                    node_id: r.node_id(),
                    digest: r.digest.clone(),
                })
                .collect(),
            explanation: "fixture judgment".into(),
        }),
    }
}

#[tokio::test]
async fn immutable_prefixes_keep_old_tool_versions_and_detect_content_corruption() -> Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "s").await?;
    recorder.record(event(CaptureKind::Notification, None, "session/update", json!({"sessionId":"s","update":{"sessionUpdate":"tool_call","toolCallId":"test","status":"in_progress"}}))).await?;
    let cp = freeze(&store, "s").await?;
    assert!(
        cp.gaps.is_empty(),
        "an open connection alone does not invalidate a fixed prefix"
    );
    assert_eq!(cp.segments[0].setup.len(), 1);
    assert_eq!(freeze(&store, "s").await?.checkpoint_id, cp.checkpoint_id);
    let before = serde_json::to_value(
        store
            .checkpoint_content(&identity("s"), &cp.checkpoint_id)
            .await?,
    )?;
    recorder.record(event(CaptureKind::Notification, None, "session/update", json!({"sessionId":"s","update":{"sessionUpdate":"tool_call_update","toolCallId":"test","status":"completed"}}))).await?;
    assert!(
        store
            .freeze_checkpoint(&identity("s"), cp.watermark)
            .await
            .is_err()
    );
    let next = freeze(&store, "s").await?;
    assert_eq!(
        next.previous_checkpoint_id.as_deref(),
        Some(cp.checkpoint_id.as_str())
    );
    assert_eq!(
        serde_json::to_value(
            store
                .checkpoint_content(&identity("s"), &cp.checkpoint_id)
                .await?
        )?,
        before
    );
    assert!(
        store
            .checkpoint_content(&identity("other"), &cp.checkpoint_id)
            .await
            .is_err()
    );
    let node = cp.segments[0].events.last().context("tool event")?;
    events::Entity::update_many()
        .col_expr(events::Column::EventJson, Expr::value("{}"))
        .filter(events::Column::ConnectionId.eq(&node.connection_id))
        .filter(events::Column::Sequence.eq(node.sequence))
        .exec(&store.db)
        .await?;
    assert!(
        store
            .checkpoint_content(&identity("s"), &cp.checkpoint_id)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn revision_cas_retries_corrections_and_retractions_preserve_one_label() -> Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "s").await?;
    let cp = freeze(&store, "s").await?;
    let auto_input = input(&cp, "auto", None, AssessmentSource::Agentic);
    let auto = store
        .submit_assessment(&identity("s"), auto_input.clone())
        .await?;
    assert_eq!(
        store
            .submit_assessment(&identity("s"), auto_input.clone())
            .await?
            .revision_id,
        auto.revision_id
    );
    let mut collision = auto_input;
    collision.reason = "different input".into();
    assert!(
        store
            .submit_assessment(&identity("s"), collision)
            .await
            .is_err()
    );
    let human = store
        .submit_assessment(
            &identity("s"),
            input(
                &cp,
                "human",
                Some(&auto.revision_id),
                AssessmentSource::Human,
            ),
        )
        .await?;
    assert_eq!(human.supersedes.as_deref(), Some(auto.revision_id.as_str()));
    assert!(
        store
            .submit_assessment(
                &identity("s"),
                input(
                    &cp,
                    "late",
                    Some(&auto.revision_id),
                    AssessmentSource::Agentic
                )
            )
            .await
            .is_err()
    );
    let late = store
        .submit_assessment(
            &identity("s"),
            input(
                &cp,
                "later",
                Some(&human.revision_id),
                AssessmentSource::Agentic,
            ),
        )
        .await?;
    assert!(!late.selected_on_submission);
    assert_eq!(late.selection_reason, "human_revision_preserved");
    assert_eq!(
        store
            .checkpoint_family(&identity("s"))
            .await?
            .current_assessments,
        1
    );
    update(recorder.as_ref(), "s", "late negative feedback").await?;
    assert!(store.effective_assessment(&identity("s")).await?.stale);
    assert_eq!(
        store
            .checkpoint_family(&identity("s"))
            .await?
            .current_assessments,
        0
    );
    let next = freeze(&store, "s").await?;
    let next_revision = store
        .submit_assessment(
            &identity("s"),
            input(
                &next,
                "next",
                Some(&human.revision_id),
                AssessmentSource::Agentic,
            ),
        )
        .await?;
    assert!(next_revision.selected_on_submission);
    let historical = store
        .submit_assessment(
            &identity("s"),
            input(
                &cp,
                "historical",
                Some(&next_revision.revision_id),
                AssessmentSource::Human,
            ),
        )
        .await?;
    assert!(!historical.selected_on_submission);
    assert_eq!(historical.selection_reason, "historical_checkpoint");
    let mut retract = input(
        &next,
        "retract",
        Some(&next_revision.revision_id),
        AssessmentSource::Human,
    );
    retract.assessment = None;
    let retracted = store
        .submit_assessment(&identity("s"), retract.clone())
        .await?;
    assert_eq!(
        store
            .submit_assessment(&identity("s"), retract)
            .await?
            .revision_id,
        retracted.revision_id
    );
    let view = store.effective_assessment(&identity("s")).await?;
    assert!(view.assessment.is_none());
    assert_eq!(view.current_revision, Some(retracted.revision_id));
    assert_eq!(store.assessment_history(&identity("s")).await?.len(), 6);
    Ok(())
}

#[tokio::test]
async fn invalid_labels_and_out_of_prefix_citations_are_rejected() -> Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "s").await?;
    let cp = freeze(&store, "s").await?;
    for field in ["assessment", "expected_revision"] {
        let mut incomplete =
            serde_json::to_value(input(&cp, "missing", None, AssessmentSource::Human))?;
        incomplete
            .as_object_mut()
            .context("input object")?
            .remove(field);
        assert!(
            serde_json::from_value::<RevisionInput>(incomplete).is_err(),
            "{field} must be explicit"
        );
    }
    let mut bad = input(&cp, "invalid", None, AssessmentSource::Human);
    let assessment = bad.assessment.as_mut().context("assessment")?;
    assessment.scores.insert(
        "bad".into(),
        CriterionScore::Scored {
            value_ppm: 1_000_001,
        },
    );
    assert!(store.submit_assessment(&identity("s"), bad).await.is_err());
    let mut bad = input(&cp, "citation", None, AssessmentSource::Human);
    bad.assessment
        .as_mut()
        .context("assessment")?
        .evidence
        .push(EvidenceCitation {
            node_id: "missing:99".into(),
            digest: "a".repeat(64),
        });
    assert!(store.submit_assessment(&identity("s"), bad).await.is_err());
    assert!(store.assessment_history(&identity("s")).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn late_metering_changes_resources_without_changing_content_or_labels() -> Result<()> {
    use crate::metering::entities::requests;
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    seed_request(&store, "r", "principal", "s", 0, "unknown").await?;
    new_session(recorder.as_ref(), "s").await?;
    let cp = freeze(&store, "s").await?;
    let original = store
        .checkpoint_resource_history(&identity("s"), &cp.checkpoint_id)
        .await?
        .pop()
        .context("initial observation")?;
    assert_eq!(original.unpriced_requests, 1);
    requests::Entity::update_many()
        .col_expr(requests::Column::ChargeStatus, Expr::value("computed"))
        .col_expr(
            requests::Column::EstimatedChargeMicroUsd,
            Expr::value(17_i64),
        )
        .filter(requests::Column::RequestId.eq("r"))
        .exec(&store.db)
        .await?;
    seed_request(&store, "after", "principal", "s", 99, "computed").await?;
    let refreshed = store
        .observe_checkpoint_resources(&identity("s"), &cp.checkpoint_id)
        .await?;
    assert_eq!(refreshed.known_cost_micro_usd, 17);
    assert_eq!(refreshed.unassigned_request_ids, vec!["after"]);
    assert_ne!(original.observation_id, refreshed.observation_id);
    assert_eq!(
        store
            .observe_checkpoint_resources(&identity("s"), &cp.checkpoint_id)
            .await?
            .observation_id,
        refreshed.observation_id
    );
    assert_eq!(
        store
            .checkpoint_resource_history(&identity("s"), &cp.checkpoint_id)
            .await?
            .len(),
        2
    );
    requests::Entity::update_many()
        .col_expr(requests::Column::ChargeStatus, Expr::value("unknown"))
        .filter(requests::Column::RequestId.eq("r"))
        .exec(&store.db)
        .await?;
    let unknown_again = store
        .observe_checkpoint_resources(&identity("s"), &cp.checkpoint_id)
        .await?;
    assert_eq!(unknown_again.revision, 3);
    requests::Entity::update_many()
        .col_expr(requests::Column::ChargeStatus, Expr::value("computed"))
        .filter(requests::Column::RequestId.eq("r"))
        .exec(&store.db)
        .await?;
    let priced_again = store
        .observe_checkpoint_resources(&identity("s"), &cp.checkpoint_id)
        .await?;
    assert_eq!(priced_again.revision, 4);
    assert_ne!(priced_again.observation_id, refreshed.observation_id);
    assert_eq!(
        store
            .checkpoint_resource_history(&identity("s"), &cp.checkpoint_id)
            .await?
            .pop()
            .context("latest resource")?
            .known_cost_micro_usd,
        17
    );
    assert_eq!(freeze(&store, "s").await?.prefix_digest, cp.prefix_digest);
    update(recorder.as_ref(), "s", "continued").await?;
    let next = freeze(&store, "s").await?;
    assert_eq!(
        store
            .checkpoint_resource_history(&identity("s"), &next.checkpoint_id)
            .await?
            .pop()
            .context("next resource")?
            .known_cost_micro_usd,
        116
    );
    Ok(())
}

#[tokio::test]
async fn inherited_prefixes_family_unions_and_deletion_preserve_native_scope() -> Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "parent").await?;
    seed_request(&store, "root", "principal", "parent", 5, "computed").await?;
    update(recorder.as_ref(), "parent", "inherited").await?;
    let parent_boundary = freeze(&store, "parent").await?;
    recorder
        .record(event(
            CaptureKind::Request,
            Some(2),
            "session/fork",
            json!({"sessionId":"parent"}),
        ))
        .await?;
    recorder
        .record(event(
            CaptureKind::Response,
            Some(2),
            "session/fork",
            json!({"result":{"sessionId":"child"}}),
        ))
        .await?;
    seed_request(&store, "child-request", "principal", "child", 7, "computed").await?;
    update(recorder.as_ref(), "child", "child work").await?;
    update(recorder.as_ref(), "parent", "not inherited").await?;
    let child = freeze(&store, "child").await?;
    let parent = freeze(&store, "parent").await?;
    assert_eq!(child.family_id, identity("parent").key()?);
    assert_eq!(child.segments.len(), 2);
    assert_eq!(child.segments[0].watermark, parent_boundary.watermark);
    let content = store
        .checkpoint_content(&identity("child"), &child.checkpoint_id)
        .await?;
    let text = serde_json::to_string(&content)?;
    assert!(text.contains("inherited"));
    assert!(!text.contains("not inherited"));
    assert_eq!(child.segments[0].events, parent_boundary.segments[0].events);
    store
        .submit_assessment(
            &identity("parent"),
            input(&parent, "p", None, AssessmentSource::Human),
        )
        .await?;
    store
        .submit_assessment(
            &identity("child"),
            input(&child, "c", None, AssessmentSource::Human),
        )
        .await?;
    let family = store.checkpoint_family(&identity("child")).await?;
    assert_eq!(family.sessions.len(), 2);
    assert_eq!(family.current_assessments, 2);
    assert_eq!(family.requests.len(), 2);
    assert_eq!(family.known_cost_micro_usd, 12);
    assert!(!family.metering_complete);
    store.delete(&identity("parent")).await?;
    assert!(
        store
            .checkpoint_content(&identity("child"), &child.checkpoint_id)
            .await
            .is_err()
    );
    assert!(
        store
            .assessment_history(&identity("child"))
            .await?
            .is_empty()
    );
    assert!(
        store
            .effective_assessment(&identity("child"))
            .await?
            .assessment
            .is_none()
    );
    assert!(resources::Entity::find().all(&store.db).await?.is_empty());
    Ok(())
}

#[tokio::test]
async fn concurrent_creation_and_revision_selection_are_durable() -> Result<()> {
    let dir = tempfile::tempdir()?;
    let url = format!(
        "sqlite://{}?mode=rwc",
        dir.path().join("capture.db").display()
    );
    let db = crate::db::connect(&url).await?;
    crate::db::run_migrations(&db).await?;
    let store = CanonicalStore::new(db);
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "s").await?;
    let identity = identity("s");
    let (first, second) = tokio::join!(
        store.freeze_checkpoint(&identity, 1),
        store.freeze_checkpoint(&identity, 1)
    );
    let cp = first?;
    assert_eq!(cp.checkpoint_id, second?.checkpoint_id);
    assert_eq!(store.checkpoints(&identity).await?.len(), 1);
    let (a, b) = tokio::join!(
        store.submit_assessment(&identity, input(&cp, "a", None, AssessmentSource::Human)),
        store.submit_assessment(&identity, input(&cp, "b", None, AssessmentSource::Human))
    );
    assert_ne!(
        a.is_ok(),
        b.is_ok(),
        "only one submission may replace the expected empty selection"
    );
    assert_eq!(store.assessment_history(&identity).await?.len(), 1);
    let selected = store
        .effective_assessment(&identity)
        .await?
        .current_revision;
    let (frozen, appended) = tokio::join!(
        store.freeze_checkpoint(&identity, 1),
        update(recorder.as_ref(), "s", "concurrent append")
    );
    appended?;
    if let Ok(frozen) = frozen {
        assert_eq!(frozen.checkpoint_id, cp.checkpoint_id);
        assert_eq!(
            store
                .checkpoint_content(&identity, &frozen.checkpoint_id)
                .await?
                .events
                .len(),
            1
        );
    }
    assert!(store.freeze_checkpoint(&identity, 1).await.is_err());
    drop(recorder);
    drop(store);
    let reopened = CanonicalStore::new(crate::db::connect(&url).await?);
    let view = reopened.effective_assessment(&identity).await?;
    assert_eq!(view.current_revision, selected);
    assert!(view.stale);
    assert_eq!(
        reopened
            .checkpoint_content(&identity, &cp.checkpoint_id)
            .await?
            .events
            .len(),
        1
    );
    Ok(())
}

#[tokio::test]
async fn missing_parent_and_capture_gaps_remain_explicit() -> Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    recorder
        .record(event(
            CaptureKind::Request,
            Some(2),
            "session/fork",
            json!({"sessionId":"unrecorded"}),
        ))
        .await?;
    recorder
        .record(event(
            CaptureKind::Response,
            Some(2),
            "session/fork",
            json!({"result":{"sessionId":"child"}}),
        ))
        .await?;
    let cp = freeze(&store, "child").await?;
    assert!(
        cp.gaps
            .iter()
            .any(|g| g.starts_with("parent_watermark_unknown:"))
    );
    assert_eq!(cp.family_id, identity("unrecorded").key()?);
    assert_eq!(cp.segments.len(), 1);
    sessions::Entity::update_many()
        .col_expr(
            sessions::Column::ParentKey,
            Expr::value(identity("child").key()?),
        )
        .filter(sessions::Column::SessionKey.eq(identity("child").key()?))
        .exec(&store.db)
        .await?;
    assert!(freeze(&store, "child").await.is_err());
    Ok(())
}

#[tokio::test]
async fn long_session_manifests_and_large_assessments_round_trip() -> Result<()> {
    let store = store().await?;
    let recorder = store.recorder(scope()).await?;
    new_session(recorder.as_ref(), "s").await?;
    for i in 0..400 {
        update(recorder.as_ref(), "s", &format!("event-{i}")).await?;
    }
    update(recorder.as_ref(), "s", &"large tool output ".repeat(8000)).await?;
    let cp = freeze(&store, "s").await?;
    assert!(serde_json::to_vec(&cp)?.len() > 65_535);
    let mut submission = input(&cp, "large", None, AssessmentSource::Human);
    submission
        .assessment
        .as_mut()
        .context("assessment")?
        .explanation = "recorded evidence ".repeat(8000);
    store
        .submit_assessment(&identity("s"), submission.clone())
        .await?;
    let stored = store
        .assessment_history(&identity("s"))
        .await?
        .pop()
        .context("revision")?;
    assert_eq!(
        serde_json::to_value(stored.input)?,
        serde_json::to_value(submission)?
    );
    assert_eq!(
        store
            .checkpoint_content(&identity("s"), &cp.checkpoint_id)
            .await?
            .events
            .len(),
        402
    );
    Ok(())
}
