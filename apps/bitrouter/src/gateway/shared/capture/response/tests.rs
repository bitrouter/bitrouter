use super::*;
use crate::gateway::shared::capture::projection::{EvaluationInput, tests::untracked};
use crate::metering::shared::tests::{fixture, resolve};

const ACTIONS: [ObservedActionClass; 8] = [
    ObservedActionClass::ReasonOrPlan,
    ObservedActionClass::InspectOrRead,
    ObservedActionClass::Mutate,
    ObservedActionClass::ExecuteOrTest,
    ObservedActionClass::WaitOrPoll,
    ObservedActionClass::AnswerOrSummarize,
    ObservedActionClass::Mixed,
    ObservedActionClass::Unknown,
];

fn ready() -> Result<Inputs> {
    let mut input = untracked(&fixture()?)?;
    input.phase = Phase::Ready;
    input.validate()?;
    Ok(input)
}

fn capture(input: Inputs) -> Result<Capture> {
    let capture = Capture::default();
    capture.initialize(input)?;
    Ok(capture)
}

fn observe(capture: &Capture, action: ObservedActionClass) -> Result<()> {
    let input = capture.snapshot().context("missing input")?;
    capture.observe_response(
        &input.execution_id,
        &input.owner,
        PredictionObservation::new(action),
    )
}

#[test]
fn only_canonical_bounded_observation_encodings_are_accepted() -> Result<()> {
    for action in ACTIONS {
        let observation = PredictionObservation::new(action);
        let encoded = ResponseObservation::from(observation);
        let json = serde_json::to_value(&encoded)?;
        let decoded: ResponseObservation = serde_json::from_value(json.clone())?;
        ensure!(decoded.typed()? == observation);
        ensure!(
            serde_json::to_vec(&serde_json::json!({"response_observation": json}))?.len()
                <= MAX_RESPONSE_BYTES
        );
        for (field, value) in [
            ("observed_action", serde_json::json!("private response")),
            ("reason_code", serde_json::json!("SECRET".repeat(1024))),
            (
                "reason_code",
                serde_json::json!("canonical_response_signal\n"),
            ),
            ("confidence_ppm", serde_json::json!(1_000_001)),
            ("confidence_ppm", serde_json::json!(0)),
        ] {
            let mut bad = json.clone();
            bad[field] = value;
            ensure!(
                serde_json::from_value::<ResponseObservation>(bad)?
                    .typed()
                    .is_err()
            );
        }
        let mut unknown = json.clone();
        unknown["provider_body"] = "SECRET".into();
        ensure!(serde_json::from_value::<ResponseObservation>(unknown).is_err());
    }
    Ok(())
}

#[test]
fn missing_old_field_stays_absent_and_unknown_is_only_an_actual_observation() -> Result<()> {
    let input = ready()?;
    let json = serde_json::to_value(&input)?;
    ensure!(json.get("response_observation").is_none());
    let restored: Inputs = serde_json::from_value(json)?;
    restored.validate()?;
    ensure!(
        !restored
            .prediction_attributes()
            .keys()
            .any(|key| is_response_attribute(key))
    );
    let capture = capture(restored)?;
    observe(&capture, ObservedActionClass::Unknown)?;
    let terminal = capture.terminal()?.context("missing terminal")?;
    let attributes = terminal.prediction_attributes();
    ensure!(attributes.get("observed_action").map(String::as_str) == Some("unknown"));
    ensure!(
        attributes
            .get("observation_confidence_ppm")
            .map(String::as_str)
            == Some("350000")
    );
    ensure!(!attributes.contains_key("action_match"));
    Ok(())
}

#[test]
fn response_merge_is_monotonic_and_cannot_change_the_original_treatment() -> Result<()> {
    let original = ready()?;
    let capture = capture(original.clone())?;
    observe(&capture, ObservedActionClass::InspectOrRead)?;
    let first = capture.snapshot().context("missing first")?;
    observe(&capture, ObservedActionClass::Unknown)?;
    ensure!(capture.snapshot().as_deref() == Some(first.as_ref()));
    observe(&capture, ObservedActionClass::Mutate)?;
    let mixed = capture.snapshot().context("missing mixed")?;
    ensure!(
        mixed.policy == original.policy
            && mixed.routing == original.routing
            && mixed.requirements == original.requirements
    );
    ensure!(
        mixed
            .response_observation
            .as_ref()
            .context("missing observation")?
            .typed()?
            .observed_action
            == ObservedActionClass::Mixed
    );
    let mut stored = original.clone();
    stored.merge(&first)?;
    let mut replacement = first.as_ref().clone();
    replacement.response_observation =
        Some(PredictionObservation::new(ObservedActionClass::Mutate).into());
    ensure!(stored.merge(&replacement).is_err());
    stored.merge(&mixed)?;
    ensure!(stored.merge(&first).is_err());
    ensure!(stored.merge(&original).is_err());
    let mut changed = mixed.as_ref().clone();
    if let Policy::Evaluated {
        treatment: Some(treatment),
        ..
    } = &mut changed.policy
    {
        treatment.decision.selected_tier = "replacement".into();
    }
    ensure!(stored.merge(&changed).is_err());
    let terminal = capture.terminal()?.context("missing terminal")?;
    stored.merge(&terminal)?;
    stored.merge(&terminal)?;
    ensure!(stored.merge(&mixed).is_err());
    ensure!(observe(&capture, ObservedActionClass::Mixed).is_err());
    ensure!(capture.terminal()?.as_deref() == Some(terminal.as_ref()));
    Ok(())
}

#[test]
fn observations_reject_wrong_identity_preparing_bypass_and_terminal_order() -> Result<()> {
    let original = ready()?;
    let capture = capture(original.clone())?;
    let observation = PredictionObservation::new(ObservedActionClass::Mutate);
    ensure!(
        capture
            .observe_response("foreign", &original.owner, observation)
            .is_err()
    );
    ensure!(
        capture
            .observe_response(&original.execution_id, "foreign", observation)
            .is_err()
    );
    ensure!(capture.snapshot().as_deref() == Some(&original));
    let mut preparing = original.clone();
    preparing.phase = Phase::Preparing;
    ensure!(observe(&self::capture(preparing)?, ObservedActionClass::Mutate).is_err());
    let mut decisions = original.clone();
    decisions.operation = ModelOperation::Decisions;
    ensure!(observe(&self::capture(decisions)?, ObservedActionClass::Mutate).is_err());
    let mut bypassed = original.clone();
    bypassed.policy = Policy::Bypassed {
        reason: Bypass::UnboundRoute,
    };
    ensure!(observe(&self::capture(bypassed)?, ObservedActionClass::Mutate).is_err());
    capture.terminal()?;
    ensure!(observe(&capture, ObservedActionClass::Mutate).is_err());
    let mut forged_treatment = original;
    if let Policy::Evaluated {
        treatment: Some(treatment),
        ..
    } = &mut forged_treatment.policy
    {
        treatment
            .attributes
            .insert("observed_action".into(), "mutate".into());
    }
    ensure!(forged_treatment.validate().is_err());
    Ok(())
}

#[test]
fn escaped_route_at_admitted_limit_fits_every_observation_and_terminal() -> Result<()> {
    let mut input = ready()?;
    input.routing.push(EvidenceItem {
        evidence_id: "route-plan:bounded".into(),
        kind: "routing.plan".into(),
        digest: format!("sha256:{}", "b".repeat(64)),
        redacted: true,
        attributes: BTreeMap::from([("judgments".into(), String::new())]),
    });
    let remaining = MAX_BYTES - 128 - MAX_RESPONSE_BYTES - serde_json::to_vec(&input)?.len();
    input.routing[0].attributes.insert(
        "judgments".into(),
        format!(
            "{}{}",
            "\"".repeat(remaining / 2),
            "x".repeat(remaining % 2)
        ),
    );
    ensure!(serde_json::to_vec(&input)?.len() == MAX_BYTES - 128 - MAX_RESPONSE_BYTES);
    for action in ACTIONS {
        let capture = capture(input.clone())?;
        capture.ready()?;
        observe(&capture, action)?;
        let terminal = capture.terminal()?.context("missing terminal")?;
        ensure!(serde_json::to_vec(terminal.as_ref())?.len() <= MAX_BYTES);
        terminal.validate()?;
    }
    input.routing[0]
        .attributes
        .get_mut("judgments")
        .context("missing receipt")?
        .push('x');
    let capture = capture(input.clone())?;
    ensure!(capture.ready().is_err());
    ensure!(capture.snapshot().as_deref() == Some(&input));
    // Old rows retain their previous validity. If they lack new headroom,
    // optional insertion is atomic and cannot prevent sealing their old facts.
    input.routing[0]
        .attributes
        .get_mut("judgments")
        .context("missing receipt")?
        .push_str(&"x".repeat(MAX_RESPONSE_BYTES - 1));
    let historical = self::capture(input)?;
    ensure!(observe(&historical, ObservedActionClass::Mutate).is_err());
    let terminal = historical
        .terminal()?
        .context("missing historical terminal")?;
    ensure!(terminal.response_observation.is_none());
    terminal.validate()?;
    Ok(())
}

#[test]
fn request_and_trajectory_projection_use_the_same_optional_response_facts() -> Result<()> {
    let mut record = fixture()?;
    resolve(
        &mut record,
        bitrouter_ai::gateway::execution::ResolutionOutcome::Terminated,
    )?;
    let mut input = untracked(&record)?;
    input.phase = Phase::Ready;
    if let Policy::Evaluated {
        treatment: Some(treatment),
        ..
    } = &mut input.policy
    {
        treatment
            .attributes
            .insert("predicted_action".into(), "mutate".into());
    }
    let capture = capture(input.clone())?;
    observe(&capture, ObservedActionClass::Mutate)?;
    let terminal = capture.terminal()?.context("missing terminal")?;
    let EvaluationInput::Request(subject) = terminal.evaluation(&record, false)? else {
        anyhow::bail!("missing request subject");
    };
    let attributes = &subject.evidence[0].attributes;
    ensure!(attributes.get("observed_action").map(String::as_str) == Some("mutate"));
    ensure!(attributes.get("action_match").map(String::as_str) == Some("true"));
    for key in [
        "success",
        "request_duration_ms",
        "latency.ms",
        "quality.pass",
        "cost_micro_usd",
    ] {
        ensure!(!attributes.contains_key(key));
    }
    let trajectory = terminal.trajectory_evidence(&record)?;
    ensure!(
        trajectory
            .structural
            .get("routing.observation_confidence_ppm")
            == Some(&850_000)
    );
    ensure!(
        trajectory
            .categorical
            .get("routing.observed_action")
            .map(String::as_str)
            == Some("mutate")
    );
    ensure!(
        trajectory
            .categorical
            .get("routing.observation_reason_code")
            .map(String::as_str)
            == Some("canonical_response_signal")
    );
    ensure!(
        trajectory
            .categorical
            .get("routing.action_match")
            .map(String::as_str)
            == Some("true")
    );
    ensure!(terminal.policy == input.policy);
    Ok(())
}

#[test]
fn action_match_requires_known_original_prediction_and_actual_observation() -> Result<()> {
    for (predicted, observed, expected) in [
        ("mutate", ObservedActionClass::Mutate, Some("true")),
        ("mutate", ObservedActionClass::InspectOrRead, Some("false")),
        ("unknown", ObservedActionClass::Mutate, None),
        ("mutate", ObservedActionClass::Unknown, None),
    ] {
        let mut input = ready()?;
        if let Policy::Evaluated {
            treatment: Some(treatment),
            ..
        } = &mut input.policy
        {
            treatment
                .attributes
                .insert("predicted_action".into(), predicted.into());
        }
        let capture = capture(input)?;
        observe(&capture, observed)?;
        let terminal = capture.terminal()?.context("missing terminal")?;
        ensure!(
            terminal
                .prediction_attributes()
                .get("action_match")
                .map(String::as_str)
                == expected
        );
    }
    Ok(())
}

#[test]
fn response_projection_stays_within_reserved_future_settlement_event() -> Result<()> {
    use crate::trajectory::types::{
        TRAJECTORY_SCHEMA_VERSION, TrajectoryEvent, TrajectoryEventKind, validate_event,
    };

    let record = fixture()?;
    let mut input = untracked(&record)?;
    input.phase = Phase::Ready;
    if let Policy::Evaluated {
        treatment: Some(treatment),
        ..
    } = &mut input.policy
    {
        treatment
            .attributes
            .insert("predicted_action".into(), "mutate".into());
    }
    input.routing.push(EvidenceItem {
        evidence_id: "\\".repeat(512),
        kind: "routing.assessment".into(),
        digest: format!("sha256:{}", "c".repeat(64)),
        redacted: true,
        attributes: ["model", "requested_model", "judgments", "confidence_kind"]
            .into_iter()
            .map(|key| (key.into(), String::new()))
            .collect(),
    });
    let limit = MAX_BYTES - 128 - MAX_RESPONSE_BYTES;
    for key in ["model", "requested_model", "judgments", "confidence_kind"] {
        let remaining = limit - serde_json::to_vec(&input)?.len();
        let escaped = (remaining / 2).min(8192);
        let plain = usize::from(escaped < 8192 && remaining % 2 == 1);
        input.routing[0].attributes.insert(
            key.into(),
            format!("{}{}", "\\".repeat(escaped), "x".repeat(plain)),
        );
    }
    ensure!(serde_json::to_vec(&input)?.len() == limit);
    let mut absent = input.clone();
    absent.seal()?;
    let baseline = serde_json::to_vec(&absent.trajectory_evidence(&record)?)?.len();
    for action in ACTIONS {
        let capture = capture(input.clone())?;
        capture.ready()?;
        observe(&capture, action)?;
        let terminal = capture.terminal()?.context("missing terminal")?;
        let evidence = terminal.trajectory_evidence(&record)?;
        // The four response fields fit in the already reserved event. No new
        // receipt, event, reference, or settlement obligation is introduced.
        ensure!(serde_json::to_vec(&evidence)?.len() <= baseline + 512);
        let mut event = TrajectoryEvent {
            schema_version: TRAJECTORY_SCHEMA_VERSION,
            event_id: "\\".repeat(512),
            owner_user_id: "\\".repeat(512),
            episode_id: "\\".repeat(512),
            request_id: Some("\\".repeat(512)),
            sequence: u64::MAX,
            kind: TrajectoryEventKind::RequestSettled,
            evidence,
            captured_at: "2026-10-21T00:00:00.999999999Z".into(),
            content_digest: String::new(),
        };
        event.content_digest = event.semantic_digest()?;
        validate_event(&event)?;
        ensure!(
            serde_json::to_vec(&event)?.len()
                <= crate::trajectory::shared::MAX_GUARDED_SETTLEMENT_BYTES
        );
    }
    Ok(())
}
