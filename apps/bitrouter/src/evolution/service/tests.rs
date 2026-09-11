use super::*;
mod revisions;
use bitrouter_sdk::acp::capture::{CaptureDirection, CaptureEvent, CaptureKind, CapturePort};
use serde_json::json;

use crate::acp_trajectory::{CanonicalStore, RecordingScope};
use crate::evolution::bandit::{
    BanditConfig, EffectiveObservations, Observation, Recommendation, draw_assignment, plan,
};

async fn setup() -> Result<(EvolutionService, CanonicalStore)> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    Ok((
        EvolutionService::new(db.clone(), "owner")?,
        CanonicalStore::new(db),
    ))
}

async fn new_session(canonical: &CanonicalStore, name: &str) -> Result<SessionIdentity> {
    new_session_with_updates(canonical, name, &[]).await
}

async fn new_session_with_updates(
    canonical: &CanonicalStore,
    name: &str,
    updates: &[serde_json::Value],
) -> Result<SessionIdentity> {
    let identity = SessionIdentity {
        owner: "owner".into(),
        source: "test-acp".into(),
        native_session_id: name.into(),
    };
    let recorder = canonical
        .recorder(RecordingScope {
            owner: identity.owner.clone(),
            source: identity.source.clone(),
            controller_instance_id: Some("controller".into()),
            route_scope_id: Some("owner".into()),
        })
        .await?;
    for (kind, payload) in [
        (CaptureKind::Request, json!({"cwd":"/fixture"})),
        (CaptureKind::Response, json!({"result":{"sessionId":name}})),
    ] {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Client,
                kind,
                call_id: Some(1),
                method: "session/new".into(),
                payload,
            })
            .await?;
    }
    for update in updates {
        recorder
            .record(CaptureEvent {
                direction: CaptureDirection::Agent,
                kind: CaptureKind::Notification,
                call_id: None,
                method: "session/update".into(),
                payload: json!({"sessionId":name,"update":update}),
            })
            .await?;
    }
    recorder.record(CaptureEvent { direction: CaptureDirection::Client, kind: CaptureKind::Request,
        call_id: Some(2), method: "session/prompt".into(),
        payload: json!({"sessionId":name,"prompt":[{"type":"text","text":"Make a small change."}]}) }).await?;
    Ok(identity)
}

fn definition(id: &str, selector: &str) -> BlockDefinition {
    BlockDefinition {
        block_id: id.into(),
        source: "test-acp".into(),
        rationale: "Compare compatible complete routes.".into(),
        rules: vec![crate::evolution::control::BlockRule {
            selector: selector.into(),
            fingerprint: None,
            baseline_route: selector.into(),
            challenger_route: format!("{selector}-candidate"),
        }],
        independence_rationale: "Fixture blocks have separate generated potential outcomes.".into(),
        dependencies: BTreeMap::new(),
        measurement_contract: "fixture-contract".into(),
        batch_sessions: 32,
        bandit: BanditConfig {
            initial_exposure_ppm: 500_000,
            maximum_pending_challenger: 2,
            ..BanditConfig::default()
        },
    }
}

fn context(selector: &str) -> DecisionContext {
    DecisionContext {
        selector: selector.into(),
        fingerprint: "short:text".into(),
    }
}

fn dependencies() -> BTreeMap<String, String> {
    BTreeMap::from([
        ("a".into(), "routes-a".into()),
        ("b".into(), "routes-b".into()),
    ])
}

#[tokio::test]
async fn assignment_survives_restart_and_duplicate_intents_do_not_resample() -> Result<()> {
    let (service, canonical) = setup().await?;
    service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    service
        .register(definition("b", "secondary"), "routes-b".into())
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let identity = new_session(&canonical, "sticky").await?;
    let first = service
        .select(&identity, "request-1", context("primary"), &dependencies())
        .await?
        .context("missing intent")?;
    assert_eq!(
        first.assignments.len(),
        2,
        "a nonmatching independent block retains its assignment intent"
    );
    let restarted = EvolutionService::new(service.store.db.clone(), "owner")?;
    let retry = restarted
        .select(&identity, "request-1", context("primary"), &dependencies())
        .await?
        .context("missing replay")?;
    assert_eq!(serde_json::to_value(&first)?, serde_json::to_value(&retry)?);
    let next = restarted
        .select(&identity, "request-2", context("primary"), &dependencies())
        .await?
        .context("missing next intent")?;
    assert_eq!(first.selected_route, next.selected_route);
    assert_eq!(first.assignments, next.assignments);
    assert_eq!(restarted.intents(&identity).await?.len(), 2);
    let enrollment = restarted
        .enrollment(&identity)
        .await?
        .context("missing enrollment")?;
    for assignment in enrollment.assignments.values() {
        assert_eq!(
            assignment.arm,
            draw_assignment(assignment.challenger_propensity_ppm, assignment.random_seed)?
        );
        assert_eq!(assignment.reference_blocks.len(), 2);
    }
    assert_eq!(restarted.state().await?.next_assignment_sequence, 2);
    assert!(
        restarted
            .select(
                &identity,
                "request-1",
                context("different"),
                &dependencies()
            )
            .await
            .is_err()
    );
    assert!(
        EvolutionService::new(service.store.db.clone(), "other")?
            .enrollment(&identity)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn concurrent_admission_respects_the_pending_limit_and_closes_the_cohort() -> Result<()> {
    let (service, canonical) = setup().await?;
    service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let mut identities = Vec::new();
    for i in 0..40 {
        identities.push(new_session(&canonical, &format!("parallel-{i}")).await?);
    }
    let mut workers = tokio::task::JoinSet::new();
    for (i, identity) in identities.into_iter().enumerate() {
        let worker = EvolutionService::new(service.store.db.clone(), "owner")?;
        workers.spawn(async move {
            worker
                .select(
                    &identity,
                    &format!("request-{i}"),
                    context("primary"),
                    &dependencies(),
                )
                .await
        });
    }
    while let Some(result) = workers.join_next().await {
        result??;
    }
    let state = service.state().await?;
    let block = state.blocks.get("a").context("missing block")?;
    assert!(block.batch.closed);
    assert!(block.batch.members.len() <= 32);
    assert!(
        block
            .batch
            .members
            .values()
            .filter(|arm| **arm == Arm::Challenger)
            .count()
            <= 2
    );
    let enrolled = service
        .store
        .list::<SessionEnrollment>(SESSION_KIND)
        .await?;
    assert_eq!(enrolled.len(), 40);
    assert_eq!(
        enrolled
            .iter()
            .filter(|(_, _, e)| e.assignments.contains_key("a"))
            .count(),
        block.batch.members.len()
    );
    Ok(())
}

#[tokio::test]
async fn mode_changes_fence_jobs_but_only_off_revokes_existing_trials() -> Result<()> {
    let (service, canonical) = setup().await?;
    service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    let initial = service.set_mode(EvolutionMode::Manual, None).await?;
    let identity = new_session(&canonical, "mode").await?;
    let before = service
        .select(&identity, "before", context("primary"), &dependencies())
        .await?
        .context("missing before")?;
    let automatic = service
        .set_mode(EvolutionMode::Automatic, Some("judge-1".into()))
        .await?;
    assert!(automatic.mode_epoch > initial.mode_epoch);
    assert_eq!(automatic.trial_epoch, initial.trial_epoch);
    let after = service
        .select(&identity, "after", context("primary"), &dependencies())
        .await?
        .context("missing after")?;
    assert_eq!(before.selected_route, after.selected_route);
    let changed_model = service
        .set_mode(EvolutionMode::Automatic, Some("judge-2".into()))
        .await?;
    assert!(changed_model.mode_epoch > automatic.mode_epoch);
    service.set_mode(EvolutionMode::Off, None).await?;
    let stopped = service
        .select(&identity, "off", context("primary"), &dependencies())
        .await?
        .context("missing stopped")?;
    assert_eq!(stopped.selected_route, "primary");
    assert_eq!(stopped.reason, "evolution_off");
    assert_eq!(
        stopped.assignments, before.assignments,
        "withdrawal does not erase assignment intent"
    );
    service.set_mode(EvolutionMode::Manual, None).await?;
    let resumed = service
        .select(&identity, "resumed", context("primary"), &dependencies())
        .await?
        .context("missing resumed")?;
    assert_eq!(
        resumed.selected_route, "primary",
        "off/on cannot silently reenroll an old session"
    );
    assert_eq!(service.state().await?.next_assignment_sequence, 1);
    Ok(())
}

#[tokio::test]
async fn adoption_keeps_the_existing_control_arm_and_applies_to_new_sessions() -> Result<()> {
    let (service, canonical) = setup().await?;
    service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    let nonparticipant = new_session(&canonical, "before-experiment").await?;
    service
        .select(
            &nonparticipant,
            "preexisting",
            context("primary"),
            &dependencies(),
        )
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let identity = new_session(&canonical, "control").await?;
    service
        .select(&identity, "initial", context("primary"), &dependencies())
        .await?;
    let key = identity.key()?;
    let (revision, _): (_, SessionEnrollment) = service
        .store
        .get(SESSION_KIND, &key)
        .await?
        .context("missing enrollment")?;
    // Establish a deterministic persisted control-arm fixture. The sampling
    // distribution itself is tested separately; this test targets adoption's
    // effect on a previously randomized session and on a new session.
    service
        .store
        .update(
            SESSION_KIND,
            &key,
            revision,
            |enrollment: &mut SessionEnrollment| {
                let assignment = enrollment
                    .assignments
                    .get_mut("a")
                    .context("missing assignment")?;
                let seed = (0..100)
                    .find(|seed| {
                        draw_assignment(assignment.challenger_propensity_ppm, *seed)
                            .is_ok_and(|arm| arm == Arm::Baseline)
                    })
                    .context("missing deterministic baseline seed")?;
                assignment.random_seed = seed;
                assignment.arm = Arm::Baseline;
                Ok(())
            },
        )
        .await?;
    let (revision, _): (_, ControlState) = service
        .store
        .get(CONTROL_KIND, CONTROL_KEY)
        .await?
        .context("missing control")?;
    service
        .store
        .update(
            CONTROL_KIND,
            CONTROL_KEY,
            revision,
            |state: &mut ControlState| {
                let block = state.blocks.get_mut("a").context("missing block")?;
                block.batch.members.insert(key.clone(), Arm::Baseline);
                block.assigned_challenger_sessions = 0;
                block.status = BlockStatus::Adopted;
                state.generation += 1;
                Ok(())
            },
        )
        .await?;
    service.set_mode(EvolutionMode::Off, None).await?;
    let old = service
        .select(
            &identity,
            "old-after-adoption",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("missing old intent")?;
    assert_eq!(old.selected_route, "primary");
    assert_eq!(old.reason, "retained_experiment_baseline");
    let prior = service
        .select(
            &nonparticipant,
            "preexisting-after-adoption",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("missing preexisting intent")?;
    assert_eq!(prior.selected_route, "primary");
    assert_eq!(prior.reason, "retained_preexisting_baseline");
    let fresh = new_session(&canonical, "after-adoption").await?;
    let next = service
        .select(
            &fresh,
            "new-after-adoption",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("missing new intent")?;
    assert_eq!(next.selected_route, "primary-candidate");
    assert_eq!(next.reason, "adopted_baseline");
    assert!(
        next.assignments.is_empty(),
        "off mode does not enroll another trial"
    );
    let repeated = service
        .select(
            &fresh,
            "continued-after-adoption",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("missing continued intent")?;
    assert_eq!(repeated.selected_route, "primary-candidate");
    assert!(
        repeated.monitoring.is_empty(),
        "Off does not start feedback monitoring"
    );
    service.set_mode(EvolutionMode::Manual, None).await?;
    let old_again = service
        .select(
            &fresh,
            "continued-after-reenable",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("missing continued intent")?;
    assert!(
        old_again.monitoring.is_empty(),
        "do not enroll a partly observed session retroactively"
    );
    let monitored = new_session(&canonical, "monitored-after-adoption").await?;
    let first = service
        .select(
            &monitored,
            "monitor-first",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("missing monitored intent")?;
    let second = EvolutionService::new(service.store.db.clone(), "owner")?
        .select(
            &monitored,
            "monitor-second",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("missing restarted intent")?;
    assert!(first.assignments.is_empty());
    assert_eq!(first.monitoring.len(), 1);
    assert_eq!(first.monitoring, second.monitoring);
    assert_eq!(second.selected_route, "primary-candidate");
    Ok(())
}

#[tokio::test]
async fn adapter_setup_notifications_preserve_first_request_admission() -> Result<()> {
    let cases = [
        (
            "setup",
            vec![
                json!({"sessionUpdate":"available_commands_update","availableCommands":[]}),
                json!({"sessionUpdate":"config_option_update","configOptions":[]}),
                json!({"sessionUpdate":"current_mode_update","currentModeId":"code"}),
                json!({"sessionUpdate":"session_info_update","_meta":{"codex":{"threadStatus":{"type":"active","activeFlags":[]}}}}),
            ],
            true,
        ),
        (
            "assistant",
            vec![
                json!({"sessionUpdate":"agent_message_chunk","content":{"type":"text","text":"Already produced an answer."}}),
            ],
            false,
        ),
        (
            "tool",
            vec![json!({"sessionUpdate":"tool_call","toolCallId":"read","title":"Read a file"})],
            false,
        ),
        (
            "usage",
            vec![json!({"sessionUpdate":"usage_update","used":10,"size":100})],
            false,
        ),
        (
            "unknown",
            vec![json!({"sessionUpdate":"future_update"})],
            false,
        ),
        (
            "malformed-setup",
            vec![json!({"sessionUpdate":"available_commands_update"})],
            false,
        ),
    ];
    for (name, updates, eligible) in cases {
        let (service, canonical) = setup().await?;
        service
            .register(definition("a", "primary"), "routes-a".into())
            .await?;
        service.set_mode(EvolutionMode::Manual, None).await?;
        let identity = new_session_with_updates(&canonical, name, &updates).await?;
        let first = service
            .select(&identity, "first", context("primary"), &dependencies())
            .await?
            .context("missing intent")?;
        assert_eq!(!first.assignments.is_empty(), eligible, "{name}");
        let continued = service
            .select(&identity, "second", context("primary"), &dependencies())
            .await?
            .context("missing continued intent")?;
        assert_eq!(first.assignments, continued.assignments, "{name}");
        let transcript = canonical.transcript(&identity).await?;
        assert_eq!(
            transcript
                .events
                .iter()
                .filter(|node| node.event.method == "session/update")
                .count(),
            updates.len(),
            "setup notifications must remain in canonical evidence"
        );
    }
    Ok(())
}

#[tokio::test]
async fn already_started_and_disabled_sessions_are_never_enrolled_late() -> Result<()> {
    let (service, canonical) = setup().await?;
    let identity = new_session(&canonical, "late").await?;
    assert!(
        service
            .select(&identity, "disabled", context("primary"), &dependencies())
            .await?
            .is_none()
    );
    assert!(service.enrollment(&identity).await?.is_none());
    service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    service
        .select(
            &identity,
            "before-enabled",
            context("primary"),
            &dependencies(),
        )
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    service
        .select(
            &identity,
            "after-enabled",
            context("primary"),
            &dependencies(),
        )
        .await?;
    assert!(
        service
            .enrollment(&identity)
            .await?
            .context("missing enrollment")?
            .assignments
            .is_empty()
    );
    canonical.delete(&identity).await?;
    assert!(
        service
            .select(&identity, "deleted", context("primary"), &dependencies())
            .await
            .is_err()
    );
    Ok(())
}

fn favorable_observations() -> Result<EffectiveObservations> {
    let mut observations = EffectiveObservations::default();
    for i in 0..80 {
        let arm = if i % 2 == 0 {
            Arm::Baseline
        } else {
            Arm::Challenger
        };
        observations.replace(Observation {
            session_key: i.to_string(),
            assignment_sequence: i,
            family_id: i.to_string(),
            revision: "v1".into(),
            measurement_contract: "fixture-contract".into(),
            arm,
            quality: Some(0.95),
            total_cost_micro_usd: Some(if arm == Arm::Baseline { 1000 } else { 100 }),
            latency_ms: Some(if arm == Arm::Baseline { 1000 } else { 500 }),
            severe_violation: false,
        })?;
    }
    Ok(observations)
}

#[test]
fn legacy_allocation_waits_for_same_evidence_revalidation_exactly_once() -> Result<()> {
    for stale_field in ["learner_v1", "learner_v2", "config", "measurement"] {
        let mut state = ControlState::default();
        state.set_mode(EvolutionMode::Manual, None)?;
        let definition = definition("a", "primary");
        let config = definition.bandit.clone();
        state.register(definition, "routes-a".into())?;
        state
            .reserve_trial("a", "existing-session", 17)?
            .context("existing allocation missing")?;
        let observations = EffectiveObservations::default();
        let current = plan(&config, &observations, "fixture-contract", Some(20_000), 31)?;
        assert!(state.apply_plan("a", current.clone(), false)?);
        let block = state.blocks.get_mut("a").context("block missing")?;
        block.last_exposure_ppm = 20_000;
        let old = block.plan.as_mut().context("plan missing")?;
        match stale_field {
            "learner_v1" => old.learner_version = "checkpoint-normal-inverse-gamma-ts-v1".into(),
            "learner_v2" => old.learner_version = "checkpoint-normal-inverse-gamma-ts-v2".into(),
            "config" => old.config_digest = "legacy-config".into(),
            _ => old.measurement_contract = "legacy-measurement".into(),
        }
        let original_batch = serde_json::to_value(&block.batch)?;
        assert!(
            state
                .reserve_trial("a", "before-revalidation", 19)?
                .is_none()
        );
        assert!(state.apply_plan("a", current, false)?);
        let generation = state.generation;
        let repeated = plan(&config, &observations, "fixture-contract", Some(20_000), 99)?;
        assert!(!state.apply_plan("a", repeated, false)?);
        assert_eq!(state.generation, generation);
        let block = state.blocks.get("a").context("block missing")?;
        assert_eq!(serde_json::to_value(&block.batch)?, original_batch);
        assert_eq!(block.last_exposure_ppm, 20_000);
        assert!(
            state
                .reserve_trial("a", "after-revalidation", 19)?
                .is_some()
        );
        let mut different_draw = plan(
            &config,
            &observations,
            "fixture-contract",
            Some(20_000),
            101,
        )?;
        different_draw.recommendation = Recommendation::Promote;
        state
            .blocks
            .get_mut("a")
            .context("block missing")?
            .batch
            .closed = true;
        assert!(
            !state.apply_plan("a", different_draw, true)?,
            "same evidence cannot replace Explore with Promote through different draws"
        );
        state.blocks.get_mut("a").context("block missing")?.status = BlockStatus::RolledBack;
        assert!(state.reserve_trial("a", "after-rollback", 21)?.is_none());
    }
    Ok(())
}

#[test]
fn resolved_unknown_feedback_keeps_admission_held_until_correction() -> Result<()> {
    let mut state = ControlState::default();
    state.set_mode(EvolutionMode::Manual, None)?;
    let mut block = definition("a", "primary");
    block.bandit = BanditConfig::default();
    block.batch_sessions = 10_000;
    let config = block.bandit.clone();
    state.register(block, "routes-a".into())?;
    let mut data = EffectiveObservations::default();
    for seed in 0..1000 {
        let key = format!("session-{seed}");
        let Some(allocation) = state.reserve_trial("a", &key, seed)? else {
            break;
        };
        data.replace(Observation {
            session_key: key.clone(),
            assignment_sequence: allocation.assignment_sequence,
            family_id: key,
            revision: "assessed-but-unknown".into(),
            measurement_contract: "fixture-contract".into(),
            arm: allocation.arm,
            quality: (allocation.arm == Arm::Baseline).then_some(0.95),
            total_cost_micro_usd: Some(if allocation.arm == Arm::Baseline {
                10000
            } else {
                1000
            }),
            latency_ms: Some(1000),
            severe_violation: false,
        })?;
    }
    let held = plan(&config, &data, "fixture-contract", None, 41)?;
    assert_eq!(held.recommendation, Recommendation::Hold);
    assert_eq!(held.challenger_propensity_ppm, 0);
    assert!(state.apply_plan("a", held, true)?);
    let block = state.blocks.get("a").context("missing block")?;
    assert!(!block.batch.closed);
    assert!(block.last_exposure_ppm > 0);
    let sequence = state.next_assignment_sequence;
    assert!(state.reserve_trial("a", "while-unknown", 23)?.is_none());
    assert_eq!(state.next_assignment_sequence, sequence);

    let corrected = data
        .sessions
        .values_mut()
        .find(|observation| observation.arm == Arm::Challenger)
        .context("missing candidate observation")?;
    let candidate_seed: u64 = corrected
        .session_key
        .strip_prefix("session-")
        .context("missing seed prefix")?
        .parse()?;
    corrected.quality = Some(0.95);
    corrected.revision = "one-corrected".into();
    let partial = plan(&config, &data, "fixture-contract", None, 41)?;
    assert_eq!(partial.challenger.incomplete_sessions, 3);
    assert!(state.apply_plan("a", partial, false)?);
    let next = state
        .reserve_trial("a", "next-cohort", candidate_seed)?
        .context("one candidate slot should be available")?;
    assert_eq!(next.arm, Arm::Challenger);
    assert!(
        state
            .reserve_trial("a", "would-exceed-global-pending", candidate_seed)?
            .is_none()
    );
    data.replace(Observation {
        session_key: "next-cohort".into(),
        assignment_sequence: next.assignment_sequence,
        family_id: "next-cohort".into(),
        revision: "pending".into(),
        measurement_contract: "fixture-contract".into(),
        arm: next.arm,
        quality: None,
        total_cost_micro_usd: Some(1000),
        latency_ms: Some(1000),
        severe_violation: false,
    })?;

    for observation in data.sessions.values_mut() {
        observation.quality = Some(0.95);
        observation.revision = "corrected".into();
    }
    let resumed = plan(&config, &data, "fixture-contract", None, 41)?;
    assert_eq!(resumed.recommendation, Recommendation::Explore);
    assert!(state.apply_plan("a", resumed, true)?);
    assert!(state.reserve_trial("a", "after-correction", 23)?.is_some());
    Ok(())
}

#[test]
fn publication_preserves_other_blocks_and_replayed_evidence_cannot_expand() -> Result<()> {
    let mut state = ControlState::default();
    state.set_mode(EvolutionMode::Manual, None)?;
    let mut a = definition("a", "primary");
    for prior in [&mut a.bandit.baseline, &mut a.bandit.challenger] {
        // Explicitly low-noise synthetic fixture, not a production calibration.
        prior.quality.observation_variance = 0.0025;
        prior.log_cost.observation_variance = 0.05;
        prior.log_latency.observation_variance = 0.05;
    }
    state.register(a, "routes-a".into())?;
    state.register(definition("b", "secondary"), "routes-b".into())?;
    let b_before = serde_json::to_value(state.blocks.get("b"))?;
    let a = state.blocks.get("a").context("missing block")?;
    let p = plan(
        &a.definition.bandit,
        &favorable_observations()?,
        "fixture-contract",
        None,
        41,
    )?;
    assert_eq!(p.recommendation, Recommendation::Promote);
    let baseline_seed = (0..100)
        .find(|seed| draw_assignment(500_000, *seed).ok() == Some(Arm::Baseline))
        .context("baseline seed missing")?;
    state
        .reserve_trial("a", "before-upgrade", baseline_seed)?
        .context("allocation missing")?;
    let mut legacy = p.clone();
    legacy.learner_version = "checkpoint-normal-inverse-gamma-ts-v1".into();
    state.blocks.get_mut("a").context("block missing")?.plan = Some(legacy);
    assert!(
        state
            .reserve_trial("a", "incompatible-plan", baseline_seed)?
            .is_none()
    );
    assert!(state.apply_plan("a", p.clone(), true)?);
    assert_eq!(
        state.blocks.get("a").context("block missing")?.status,
        BlockStatus::Exploring,
        "staging an eligible plan must not promote an open cohort"
    );
    assert!(
        state
            .reserve_trial("a", "after-upgrade", baseline_seed)?
            .is_some()
    );
    let staged_generation = state.generation;
    assert!(!state.apply_plan("a", p.clone(), true)?);
    assert_eq!(state.generation, staged_generation);
    state
        .blocks
        .get_mut("a")
        .context("missing block")?
        .batch
        .closed = true;
    assert!(!state.apply_plan("a", p.clone(), false)?);
    assert_eq!(
        state.blocks.get("a").context("block missing")?.status,
        BlockStatus::Exploring,
        "staged promotion still requires resolved cohort feedback"
    );
    assert!(state.apply_plan("a", p.clone(), true)?);
    let generation = state.generation;
    assert!(!state.apply_plan("a", p, true)?);
    assert_eq!(generation, state.generation);
    assert_eq!(b_before, serde_json::to_value(state.blocks.get("b"))?);
    assert_eq!(state.publications.len(), 2);
    let adopted_revision = state
        .blocks
        .get("a")
        .context("missing adopted block")?
        .revision
        .clone();
    let mut revised_evidence = favorable_observations()?;
    revised_evidence
        .sessions
        .get_mut("0")
        .context("missing trial observation")?
        .revision = "corrected-with-same-score".into();
    let updated = plan(
        &state
            .blocks
            .get("a")
            .context("missing block")?
            .definition
            .bandit,
        &revised_evidence,
        "fixture-contract",
        None,
        41,
    )?;
    assert_eq!(updated.recommendation, Recommendation::Promote);
    assert!(state.apply_plan("a", updated, true)?);
    assert_eq!(
        state.blocks.get("a").context("missing block")?.revision,
        adopted_revision
    );
    assert_eq!(
        state.publications.last().map(|p| p.action.as_str()),
        Some("adoption_revalidated")
    );
    assert_eq!(b_before, serde_json::to_value(state.blocks.get("b"))?);
    state.set_mode(EvolutionMode::Off, None)?;
    assert_eq!(
        state
            .blocks
            .get("a")
            .context("missing adopted block")?
            .status,
        BlockStatus::Adopted
    );
    Ok(())
}
