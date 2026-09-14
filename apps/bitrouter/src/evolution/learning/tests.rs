use super::*;
use crate::acp_trajectory::checkpoint::types::{AssessmentSource, CriterionScore};
use crate::acp_trajectory::{Recorder, RecordingScope, SessionIdentity};
use crate::evolution::{
    control::{BlockDefinition, BlockRule},
    rubric::{Applicability, RUBRIC_VERSION, RubricItem, library},
    scoring,
    service::DecisionContext,
};
use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureEvent, CapturePort};
use std::sync::Arc;

async fn fixture() -> Result<(
    EvolutionService,
    CanonicalStore,
    Arc<Recorder>,
    SessionIdentity,
)> {
    fixture_with_inventory(false).await
}

async fn fixture_with_inventory(
    acknowledge: bool,
) -> Result<(
    EvolutionService,
    CanonicalStore,
    Arc<Recorder>,
    SessionIdentity,
)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let service = EvolutionService::new(db.clone(), "owner")?;
    let inventory = crate::evolution::inventory::GatewayInventory::new(db.clone());
    let canonical = CanonicalStore::new(db);
    service
        .register(
            BlockDefinition {
                block_id: "a".into(),
                source: "fixture".into(),
                rationale: "Recorded-observation fixture.".into(),
                rules: vec![BlockRule {
                    selector: "baseline".into(),
                    fingerprint: None,
                    baseline_route: "baseline".into(),
                    challenger_route: "candidate".into(),
                }],
                independence_rationale: "Only one block is active.".into(),
                dependencies: BTreeMap::new(),
                measurement_contract: scoring::measurement_contract(
                    AssessmentSource::Human,
                    "operator",
                    "v1",
                )?,
                batch_sessions: 1,
                bandit: Default::default(),
            },
            "routes".into(),
        )
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let identity = SessionIdentity {
        owner: "owner".into(),
        source: "fixture".into(),
        native_session_id: "s".into(),
    };
    let recorder = canonical
        .recorder(RecordingScope {
            owner: "owner".into(),
            source: "fixture".into(),
            controller_instance_id: Some("controller".into()),
            route_scope_id: Some("principal".into()),
        })
        .await?;
    if acknowledge {
        inventory
            .register_capture(recorder.connection_id(), "principal", "controller")
            .await?;
    }
    for (kind, method, id, payload) in [
        (
            CaptureKind::Request,
            "session/new",
            1,
            serde_json::json!({"cwd":"/fixture"}),
        ),
        (
            CaptureKind::Response,
            "session/new",
            1,
            serde_json::json!({"result":{"sessionId":"s"}}),
        ),
        (
            CaptureKind::Request,
            "session/prompt",
            2,
            serde_json::json!({"sessionId":"s","prompt":[{"type":"text","text":"Fix the parser and run tests. No PR."}]}),
        ),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(id),
                method: method.into(),
                payload,
            })
            .await?;
    }
    service
        .select(
            &identity,
            "request",
            DecisionContext {
                selector: "baseline".into(),
                fingerprint: "short:tool".into(),
            },
            &BTreeMap::from([("a".into(), "routes".into())]),
        )
        .await?;
    recorder.record(CaptureEvent { direction: CaptureDirection::Agent, kind: CaptureKind::Notification,
        call_id: None, method: "session/update".into(), payload: serde_json::json!({"sessionId":"s","update":{
            "sessionUpdate":"tool_call","toolCallId":"tests","status":"completed","title":"Parser tests",
            "rawOutput":{"stdout":"2 tests passed","exitCode":0}}}) }).await?;
    recorder
        .record(CaptureEvent {
            direction: CaptureDirection::Client,
            kind: CaptureKind::Response,
            call_id: Some(2),
            method: "session/prompt".into(),
            payload: serde_json::json!({"result":{"stopReason":"end_turn"}}),
        })
        .await?;
    Ok((service, canonical, recorder, identity))
}

async fn score(
    canonical: &CanonicalStore,
    identity: &SessionIdentity,
    value: u32,
    id: &str,
) -> Result<scoring::ScoringReceipt> {
    let head = canonical.transcript(identity).await?.session.head;
    let checkpoint = canonical.freeze_checkpoint(identity, head).await?;
    let input = scoring::prepare(canonical, identity, &checkpoint.checkpoint_id).await?;
    let citation = input
        .evidence
        .items
        .iter()
        .find(|item| item.kind == crate::evolution::evidence::EvidenceKind::ToolObservation)
        .context("missing observed tests")?
        .citation
        .clone();
    let evaluation = RubricEvaluation {
        rubric_version: RUBRIC_VERSION.into(),
        items: library()
            .into_iter()
            .map(|item| {
                let applicable = item.mandatory || item.id == "verification";
                RubricItem {
                    criterion_id: item.id.into(),
                    applicability: if applicable {
                        Applicability::Applicable
                    } else {
                        Applicability::NotApplicable
                    },
                    selection_reason:
                        "Controlled parser fixture has delivery, constraint and test obligations."
                            .into(),
                    score: if applicable {
                        CriterionScore::Scored { value_ppm: value }
                    } else {
                        CriterionScore::NotApplicable
                    },
                    evidence: vec![citation.clone()],
                    explanation: "Fixture score used only to verify revision mechanics.".into(),
                }
            })
            .collect(),
        diagnostics: vec![],
        severe_violation: false,
        violation_evidence: vec![],
        summary: "Controlled fixture assessment.".into(),
    };
    scoring::submit(
        canonical,
        identity,
        scoring::RubricSubmission {
            submission_id: id.into(),
            checkpoint_id: checkpoint.checkpoint_id,
            expected_revision: input.expected_revision,
            source: AssessmentSource::Human,
            evaluator_id: "operator".into(),
            evaluator_version: "v1".into(),
            evaluation,
        },
    )
    .await
}

fn total_families(report: &LearningReport) -> usize {
    report.plan.baseline.quality.observed_families
        + report.plan.challenger.quality.observed_families
}

#[tokio::test]
async fn metadata_revisions_keep_sampling_stable_and_invalidate_publication_snapshot() -> Result<()>
{
    let (service, canonical, _, identity) = fixture().await?;
    score(&canonical, &identity, 950_000, "original").await?;
    let before = service.learning_snapshot("a", None).await?;
    score(&canonical, &identity, 950_000, "same-outcome-new-revision").await?;
    let after = service.learning_snapshot("a", None).await?;

    // The new assessment is still a new source head: a plan constructed from
    // the previous head must fail publication fencing even with equal values.
    assert_ne!(
        before.report.plan.evidence_digest,
        after.report.plan.evidence_digest
    );
    assert_ne!(before.report.plan.plan_id, after.report.plan.plan_id);
    let tx = service.store.db.begin().await?;
    assert!(before.validate(&tx, &service.store).await.is_err());
    after.validate(&tx, &service.store).await?;
    tx.rollback().await?;

    assert_eq!(
        serde_json::to_value(&before.report.plan.baseline)?,
        serde_json::to_value(&after.report.plan.baseline)?
    );
    assert_eq!(
        serde_json::to_value(&before.report.plan.challenger)?,
        serde_json::to_value(&after.report.plan.challenger)?
    );
    assert_eq!(before.report.plan.seed, after.report.plan.seed);
    assert_eq!(
        before.report.plan.joint_benefit_probability_ppm,
        after.report.plan.joint_benefit_probability_ppm
    );
    assert_eq!(
        before.report.plan.monte_carlo_lower_ppm,
        after.report.plan.monte_carlo_lower_ppm
    );
    assert_eq!(
        before.report.plan.recommendation,
        after.report.plan.recommendation
    );

    // Common draws must not conceal a real correction or manufacture another
    // independent family from the same canonical session.
    score(&canonical, &identity, 100_000, "changed-outcome").await?;
    let corrected = service.learning_status("a").await?;
    assert_eq!(corrected.plan.seed, after.report.plan.seed);
    assert_eq!(total_families(&corrected), 1);
    let quality = |report: &LearningReport| {
        report.plan.baseline.quality.mean + report.plan.challenger.quality.mean
    };
    assert!(quality(&corrected) < quality(&after.report));
    assert_ne!(
        corrected.plan.evidence_digest,
        after.report.plan.evidence_digest
    );
    Ok(())
}

#[tokio::test]
async fn revision_keeps_canonical_evidence_separate_and_reconciles_archived_withdrawal()
-> Result<()> {
    let (service, canonical, _, identity) = fixture().await?;
    score(&canonical, &identity, 950_000, "original-score").await?;
    let (version, original): (_, ControlState) = service
        .store
        .get(CONTROL_KIND, CONTROL_KEY)
        .await?
        .context("missing control")?;
    let root = original
        .blocks
        .get("a")
        .context("missing root")?
        .experiment_id
        .clone();
    // Arrange an earlier adoption to isolate durable archive reconciliation.
    service
        .store
        .update(
            CONTROL_KIND,
            CONTROL_KEY,
            version,
            |state: &mut ControlState| {
                let block = state.blocks.get_mut("a").context("missing block")?;
                block.status = BlockStatus::Adopted;
                let mut definition = block.definition.clone();
                definition.rules[0].baseline_route = "candidate".into();
                definition.rules[0].challenger_route = "third".into();
                definition.bandit.minimum_families_per_arm = 40;
                state.revise(definition, "routes-v2".into(), &root, false)
            },
        )
        .await?;
    let current = service.learning_status("a").await?;
    assert!(current.observations.sessions.is_empty());
    assert!(!current.archived);
    assert_eq!(current.minimum_families_per_arm, Some(40));
    let archived = service.learning_status_experiment("a", Some(&root)).await?;
    assert!(archived.archived);
    assert_eq!(archived.minimum_families_per_arm, Some(20));
    assert_eq!(archived.observations.sessions.len(), 1);
    assert_eq!(total_families(&archived), 1);
    let before = service.learning_snapshot("a", Some(&root)).await?;
    score(&canonical, &identity, 100_000, "revised-score").await?;
    let tx = service.store.db.begin().await?;
    assert!(before.validate(&tx, &service.store).await.is_err());
    tx.rollback().await?;
    let reconciled = service
        .reconcile_checked("a", Some(&root), || Ok(()))
        .await?;
    assert!(reconciled.archived && reconciled.published);
    assert_eq!(reconciled.block_status, BlockStatus::RolledBack);
    let state = service.state().await?;
    assert_eq!(
        state.blocks.get("a").context("missing current")?.status,
        BlockStatus::RolledBack
    );
    assert_eq!(
        state.publications.last().map(|p| p.action.as_str()),
        Some("inherited_baseline_withdrawn")
    );
    assert!(
        !service
            .reconcile_checked("a", Some(&root), || Ok(()))
            .await?
            .published
    );
    Ok(())
}

#[tokio::test]
async fn adopted_sessions_use_separate_quality_evidence_and_a_fenced_rollback() -> Result<()> {
    let (service, canonical, _recorder, _trial) = fixture().await?;
    // Arrange a deployed block to isolate monitoring mechanics. Full trial
    // promotion and resource calibration are separate acceptance requirements.
    let (version, _): (_, ControlState) = service
        .store
        .get(CONTROL_KIND, CONTROL_KEY)
        .await?
        .context("control missing")?;
    service
        .store
        .update(
            CONTROL_KIND,
            CONTROL_KEY,
            version,
            |state: &mut ControlState| {
                state.blocks.get_mut("a").context("block missing")?.status = BlockStatus::Adopted;
                state.generation += 1;
                Ok(())
            },
        )
        .await?;
    let mut monitored = Vec::new();
    for i in 0..12 {
        let name = format!("monitor-{i}");
        let identity = SessionIdentity {
            owner: "owner".into(),
            source: "fixture".into(),
            native_session_id: name.clone(),
        };
        let recorder = canonical
            .recorder(RecordingScope {
                owner: "owner".into(),
                source: "fixture".into(),
                controller_instance_id: None,
                route_scope_id: None,
            })
            .await?;
        for (kind, call_id, method, payload) in [
            (
                CaptureKind::Request,
                1,
                "session/new",
                serde_json::json!({"cwd":"/fixture"}),
            ),
            (
                CaptureKind::Response,
                1,
                "session/new",
                serde_json::json!({"result":{"sessionId":name}}),
            ),
            (
                CaptureKind::Request,
                2,
                "session/prompt",
                serde_json::json!({"sessionId":name,"prompt":[{"type":"text","text":"Fix the parser and run tests."}]}),
            ),
        ] {
            recorder
                .record(CaptureEvent {
                    direction: CaptureDirection::Client,
                    kind,
                    call_id: Some(call_id),
                    method: method.into(),
                    payload,
                })
                .await?;
        }
        let intent = service
            .select(
                &identity,
                &format!("request-{i}"),
                DecisionContext {
                    selector: "baseline".into(),
                    fingerprint: "short:tool".into(),
                },
                &BTreeMap::from([("a".into(), "routes".into())]),
            )
            .await?
            .context("monitoring route missing")?;
        assert_eq!(intent.selected_route, "candidate");
        assert!(intent.assignments.is_empty());
        assert_eq!(intent.monitoring.len(), 1);
        recorder.record(CaptureEvent { direction: CaptureDirection::Agent, kind: CaptureKind::Notification, call_id: None, method: "session/update".into(), payload: serde_json::json!({"sessionId":name,"update":{"sessionUpdate":"tool_call","toolCallId":"tests","status":"failed","title":"Controlled parser tests","rawOutput":{"exitCode":1}}}) }).await?;
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Agent,
                kind: CaptureKind::Response,
                call_id: Some(2),
                method: "session/prompt".into(),
                payload: serde_json::json!({"result":{"stopReason":"end_turn"}}),
            })
            .await?;
        score(&canonical, &identity, 200_000, &format!("score-{i}")).await?;
        monitored.push(identity);
    }
    let before = service.learning_snapshot("a", None).await?;
    assert_eq!(
        before.report.observations.sessions.len(),
        1,
        "deployment feedback cannot inflate trial samples"
    );
    assert_eq!(before.report.monitoring_observations.sessions.len(), 12);
    assert!(before.report.monitoring.rollback);
    let corrected = monitored.first().context("monitored session missing")?;
    score(&canonical, corrected, 950_000, "correction").await?;
    let tx = service.store.db.begin().await?;
    assert!(
        before.validate(&tx, &service.store).await.is_err(),
        "a prepared alarm must revalidate corrected evidence"
    );
    tx.rollback().await?;
    let outcome = service.reconcile("a").await?;
    assert!(outcome.published && outcome.monitoring.rollback);
    assert_eq!(outcome.block_status, BlockStatus::RolledBack);
    assert_eq!(
        service
            .state()
            .await?
            .publications
            .last()
            .map(|p| p.action.as_str()),
        Some("adopted_quality_rollback")
    );
    let route = service
        .select(
            corrected,
            "after-monitoring-rollback",
            DecisionContext {
                selector: "baseline".into(),
                fingerprint: "short:tool".into(),
            },
            &BTreeMap::from([("a".into(), "routes".into())]),
        )
        .await?
        .context("rollback route missing")?;
    assert_eq!(route.selected_route, "baseline");
    assert!(
        !service.reconcile("a").await?.published,
        "withdrawal is latched"
    );
    Ok(())
}

#[tokio::test]
async fn legacy_resource_membership_requires_refresh_without_rescoring() -> Result<()> {
    let (service, canonical, _recorder, identity) = fixture_with_inventory(true).await?;
    score(&canonical, &identity, 900_000, "membership-score").await?;
    let before = canonical.effective_assessment(&identity).await?;
    let checkpoint = before.checkpoint.context("checkpoint missing")?;
    let original = canonical
        .observe_checkpoint_resources(&identity, &checkpoint.checkpoint_id)
        .await?;
    let mut legacy = serde_json::to_value(&original)?;
    legacy
        .as_object_mut()
        .context("resource must be an object")?
        .remove("membership_version");
    // Simulate an existing pre-versioned resource record. Migration must retain
    // its bytes and append a new observation instead of relabelling old history.
    let legacy_text = serde_json::to_string(&legacy)?;
    resources::Entity::update_many()
        .col_expr(
            resources::Column::ObservationJson,
            Expr::value(legacy_text.clone()),
        )
        .filter(resources::Column::ObservationId.eq(&original.observation_id))
        .exec(&service.store.db)
        .await?;
    let held = service.learning_status("a").await?;
    assert_eq!(held.observations.sessions.len(), 1);
    assert!(
        held.observations
            .sessions
            .values()
            .all(|observation| observation.quality == Some(0.9)
                && observation.total_cost_micro_usd.is_none())
    );
    assert!(
        held.unavailable
            .values()
            .flatten()
            .any(|reason| reason == "resource_membership_contract_changed")
    );
    let updated = canonical
        .observe_checkpoint_resources(&identity, &checkpoint.checkpoint_id)
        .await?;
    assert_eq!(
        updated.membership_version.as_deref(),
        Some(RESOURCE_MEMBERSHIP_VERSION)
    );
    assert!(updated.revision > original.revision);
    assert_eq!(
        updated.previous_observation_id.as_deref(),
        Some(original.observation_id.as_str())
    );
    let saved_legacy = resources::Entity::find_by_id(&original.observation_id)
        .one(&service.store.db)
        .await?
        .context("legacy resource missing")?;
    assert_eq!(saved_legacy.observation_json, legacy_text);
    assert_eq!(
        canonical
            .effective_assessment(&identity)
            .await?
            .current_revision,
        before.current_revision
    );
    assert!(
        service
            .learning_status("a")
            .await?
            .observations
            .sessions
            .values()
            .all(|observation| observation.total_cost_micro_usd == Some(0))
    );
    Ok(())
}

#[tokio::test]
async fn publication_rechecks_inventory_even_when_canonical_content_did_not_change() -> Result<()> {
    let (service, canonical, _recorder, identity) = fixture_with_inventory(true).await?;
    score(&canonical, &identity, 900_000, "coverage-score").await?;
    let cp = canonical
        .effective_assessment(&identity)
        .await?
        .checkpoint
        .context("checkpoint missing")?;
    let resources = canonical
        .observe_checkpoint_resources(&identity, &cp.checkpoint_id)
        .await?;
    assert!(
        resources.metering_complete,
        "{:?}",
        resources.gateway_coverage
    );
    let snapshot = service.learning_snapshot("a", None).await?;
    assert!(
        snapshot
            .report
            .observations
            .sessions
            .values()
            .all(|observation| observation.total_cost_micro_usd == Some(0))
    );
    let restarted = crate::evolution::inventory::GatewayInventory::new(service.store.db.clone());
    restarted
        .begin("principal", "controller", "late-unresolved", None)
        .await?;
    assert_eq!(
        canonical.transcript(&identity).await?.session.head,
        cp.watermark
    );
    let tx = service.store.db.begin().await?;
    let rejected = snapshot.validate(&tx, &service.store).await;
    assert!(
        rejected.is_err(),
        "an obsolete complete-cost snapshot must not publish"
    );
    tx.rollback().await?;
    let status = service.learning_status("a").await?;
    assert!(
        status
            .observations
            .sessions
            .values()
            .all(|observation| observation.total_cost_micro_usd.is_none())
    );
    // The selected assessment remains available; only its resource evidence is stale.
    assert!(
        canonical
            .effective_assessment(&identity)
            .await?
            .assessment
            .is_some()
    );
    Ok(())
}

#[tokio::test]
async fn corrections_replace_and_appends_withdraw_the_effective_contribution() -> Result<()> {
    let (service, canonical, recorder, identity) = fixture().await?;
    score(&canonical, &identity, 900_000, "initial").await?;
    let before = service.reconcile("a").await?;
    assert_eq!(total_families(&before), 1);
    assert!(
        before
            .observations
            .sessions
            .values()
            .all(|observation| observation.quality == Some(0.9))
    );
    assert!(
        before
            .observations
            .sessions
            .values()
            .all(|observation| observation.total_cost_micro_usd.is_none()),
        "correlated metering without complete request coverage cannot authorize promotion"
    );
    let generation = service.state().await?.generation;
    assert!(!service.reconcile("a").await?.published);
    assert_eq!(service.state().await?.generation, generation);
    score(&canonical, &identity, 100_000, "correction").await?;
    let corrected = service.learning_status("a").await?;
    assert_eq!(total_families(&corrected), 1);
    assert_eq!(corrected.observations.sessions.len(), 1);
    assert!(
        corrected
            .observations
            .sessions
            .values()
            .all(|observation| observation.quality == Some(0.1))
    );
    let snapshot = service.learning_snapshot("a", None).await?;
    recorder.record(CaptureEvent { direction: CaptureDirection::Agent, kind: CaptureKind::Notification,
        call_id: None, method: "session/update".into(), payload: serde_json::json!({"sessionId":"s","update":{
            "sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"More work followed."}}}) }).await?;
    let tx = service.store.db.begin().await?;
    assert!(
        snapshot.validate(&tx, &service.store).await.is_err(),
        "a stale prepared promotion must fail under its final locks"
    );
    tx.rollback().await?;
    let stale = service.learning_status("a").await?;
    assert_eq!(total_families(&stale), 0);
    assert_eq!(
        stale.observations.sessions.len(),
        1,
        "assignment remains in the denominator"
    );
    assert!(
        stale
            .unavailable
            .values()
            .flatten()
            .any(|reason| reason == "new_content_unassessed")
    );
    Ok(())
}

#[tokio::test]
async fn retraction_and_deletion_never_revive_an_older_success() -> Result<()> {
    let (service, canonical, _, identity) = fixture().await?;
    let scored = score(&canonical, &identity, 900_000, "initial").await?;
    let mut retraction = scored.revision.input;
    retraction.submission_id = "retract".into();
    retraction.expected_revision = Some(scored.revision.revision_id);
    retraction.assessment = None;
    retraction.reason = "Withdraw this fixture label.".into();
    canonical.submit_assessment(&identity, retraction).await?;
    assert_eq!(total_families(&service.learning_status("a").await?), 0);
    canonical.delete(&identity).await?;
    let deleted = service.learning_status("a").await?;
    assert_eq!(total_families(&deleted), 0);
    assert!(
        deleted
            .unavailable
            .values()
            .flatten()
            .any(|reason| reason == "source_deleted")
    );
    assert!(
        service
            .reconcile("a")
            .await?
            .observations
            .sessions
            .values()
            .all(|o| o.quality.is_none())
    );
    Ok(())
}

#[test]
fn active_time_merges_parallel_intervals_and_does_not_invent_missing_ends() -> Result<()> {
    let event = |connection: &str, call_id: u64, kind, second: u32| CanonicalEvent {
        node_id: format!("{connection}:{second}"),
        sequence: i64::from(second),
        captured_at: format!("2026-01-01T00:00:{second:02}Z"),
        event: CaptureEvent {
            direction: CaptureDirection::Client,
            kind,
            call_id: Some(call_id),
            method: "session/prompt".into(),
            payload: serde_json::json!({}),
        },
    };
    let mut events = vec![
        event("a", 1, CaptureKind::Request, 0),
        event("b", 1, CaptureKind::Request, 5),
        event("a", 1, CaptureKind::Response, 10),
        event("b", 1, CaptureKind::Response, 15),
    ];
    assert_eq!(prompt_active_ms(&events)?, Some(15_000));
    events.pop();
    assert_eq!(prompt_active_ms(&events)?, None);
    Ok(())
}
