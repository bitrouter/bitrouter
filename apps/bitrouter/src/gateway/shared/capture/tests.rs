use super::*;

fn input() -> Result<Inputs> {
    Inputs::new(
        Requirements::fixture(),
        "execution",
        "owner",
        ModelOperation::Generation,
    )
}

#[test]
fn incomplete_terminal_does_not_invent_a_policy_bypass() -> Result<()> {
    let mut value = input()?;
    ensure!(value.require_ready().is_err());
    value.seal()?;
    ensure!(matches!(value.policy, Policy::Pending));
    ensure!(value.phase == Phase::Terminal { route_ready: false });
    let mut invented = value.clone();
    invented.policy = Policy::Bypassed {
        reason: Bypass::UnboundRoute,
    };
    ensure!(value.merge(&invented).is_err());
    Ok(())
}

#[test]
fn capture_retains_original_ready_and_terminal_content() -> Result<()> {
    let capture = Capture::default();
    capture.initialize(input()?)?;
    capture.bypass(Bypass::UnboundRoute)?;
    capture.ready()?;
    let ready = capture.snapshot().context("missing ready capture")?;
    ready.require_ready()?;
    ensure!(capture.bypass(Bypass::FixedNative).is_err());
    ensure!(capture.snapshot().as_deref() == Some(ready.as_ref()));
    let terminal = capture.terminal()?.context("missing terminal")?;
    ensure!(terminal.phase == Phase::Terminal { route_ready: true });
    ensure!(capture.ready().is_err());
    ensure!(capture.terminal()?.as_deref() == Some(terminal.as_ref()));
    let mut stored = input()?;
    stored.merge(&ready)?;
    stored.merge(&terminal)?;
    ensure!(stored == *terminal);
    Ok(())
}

#[test]
fn changed_identity_contract_and_disappearing_receipts_fail() -> Result<()> {
    let mut original = input()?;
    for field in ["execution_id", "owner"] {
        let mut json = serde_json::to_value(&original)?;
        json[field] = "another".into();
        let changed = serde_json::from_value(json)?;
        ensure!(original.merge(&changed).is_err());
    }
    let mut changed = original.clone();
    changed.requirements.source_digest = Some(format!("sha256:{}", "a".repeat(64)));
    ensure!(original.merge(&changed).is_err());
    original.routing.push(receipt());
    ensure!(original.merge(&input()?).is_err());
    let mut changed = original.clone();
    changed.routing[0].digest = format!("sha256:{}", "b".repeat(64));
    ensure!(original.merge(&changed).is_err());
    Ok(())
}

fn receipt() -> EvidenceItem {
    EvidenceItem {
        evidence_id: "routing-assessment:fixture".into(),
        kind: "routing.assessment".into(),
        digest: format!("sha256:{}", "a".repeat(64)),
        redacted: true,
        attributes: BTreeMap::from([("judgments".into(), String::new())]),
    }
}

#[test]
fn escaped_byte_budget_reserves_terminal_headroom() -> Result<()> {
    let mut value = input()?;
    value.fixture_ready()?;
    value.routing.push(receipt());
    let base = serde_json::to_vec(&value)?.len();
    let budget = MAX_BYTES - 128;
    value.routing[0]
        .attributes
        .insert("judgments".into(), "x".repeat(budget - base));
    ensure!(serde_json::to_vec(&value)?.len() == budget);
    value.validate()?;
    let mut overflowing = value.clone();
    overflowing.routing[0]
        .attributes
        .get_mut("judgments")
        .context("missing attribute")?
        .push('"');
    ensure!(overflowing.validate().is_err());
    value.seal()?;
    ensure!(serde_json::to_vec(&value)?.len() <= MAX_BYTES);
    value.validate()?;
    Ok(())
}

#[test]
fn requirements_retain_policy_authority_independent_of_core() -> Result<()> {
    let config = crate::config::parse(
        "inherit_defaults: false\nrouters:\n  coding:\n    selection:\n      kind: policy\n      base_model: fixture:strong\n      policy: auto\n",
    )?;
    let document: bitrouter_ai::policy::Document =
        serde_saphyr::from_str(include_str!("../../../../policy/default.yaml"))?;
    let requirements = Requirements::from_config(&config, Some(&document))?;
    ensure!(requirements.source_digest.as_deref() == Some(semantic_digest(&document)?.as_str()));
    ensure!(requirements.policy_evaluation == PolicyEvaluation::SelectedUntrackedRequestSubject);
    let mut tracked = config.clone();
    tracked.trajectory.enabled = true;
    let next = Requirements::from_config(&tracked, Some(&document))?;
    ensure!(next.policy_evaluation == requirements.policy_evaluation);
    ensure!(next.trajectory == TrajectoryRequirement::Disabled);
    let mut unknown = serde_json::to_value(&requirements)?;
    unknown["format"] = 2.into();
    ensure!(
        serde_json::from_value::<Requirements>(unknown)?
            .validate()
            .is_err()
    );
    Ok(())
}

#[test]
fn unsupported_guard_requirements_and_captures_are_rejected_from_current_shape() -> Result<()> {
    let original = serde_json::to_value(input()?)?;
    let mut requirement = original.clone();
    requirement["requirements"]["trajectory"] = "guarded_policies".into();
    ensure!(serde_json::from_value::<Inputs>(requirement).is_err());

    for guarded in [false, true] {
        let mut capture = original.clone();
        capture["requirements"]["source_digest"] = format!("sha256:{}", "a".repeat(64)).into();
        capture["policy"] = serde_json::json!({
            "state": "evaluated", "policy": "auto",
            "digest": format!("sha256:{}", "a".repeat(64)),
            "guarded": guarded, "treatment": null
        });
        ensure!(serde_json::from_value::<Inputs>(capture).is_err());
    }
    let mut historical = original;
    historical["requirements"]["policy_lock_digest"] = serde_json::Value::Null;
    ensure!(serde_json::from_value::<Inputs>(historical).is_err());
    Ok(())
}

mod extra_capture_contract {
    use super::*;
    use crate::gateway::observability::decision::{
        EvalInvocation, PendingEvalDecision, PendingEvalDecisionStore,
    };
    use crate::policy_source::{PolicyRoutingSnapshot, PolicyRuntime};
    use crate::policy_table_router::{PolicyDecision, PolicyTableRouter};
    use bitrouter_ai::types::{GenerationParams, Message, Prompt, ProviderMetadata, Role};

    const PRIVATE_MARKER: &str = "PRIVATE_CAPTURE_SENTINEL";

    struct Fixture {
        requirements: Requirements,
        snapshot: PolicyRoutingSnapshot,
        router: PolicyTableRouter,
        pending: PendingEvalDecisionStore,
        guarded: bool,
    }

    fn document() -> Result<bitrouter_ai::policy::Document> {
        Ok(serde_saphyr::from_str(include_str!(
            "../../../../policy/default.yaml"
        ))?)
    }

    impl Fixture {
        fn new(document: bitrouter_ai::policy::Document) -> Result<Self> {
            let config = crate::config::parse(
                "inherit_defaults: false\nrouters:\n  coding:\n    selection:\n      kind: policy\n      base_model: fixture:strong\n      policy: auto\n",
            )?;
            let compiled = crate::model_policy::compile(&document)?;
            let definition = compiled.selections().get("auto").context("missing auto")?;
            let requirements = Requirements::from_config(&config, Some(&document))?;
            let pending = PendingEvalDecisionStore::default();
            let router = PolicyTableRouter::new(
                crate::policy_table_router::PolicyTable::from_model_selection(definition),
            )
            .with_eval_observer(
                pending.clone(),
                "auto",
                semantic_digest(&document)?,
                Default::default(),
                Some(definition.default_tier.clone()),
            );
            let runtime = PolicyRuntime::from_document(&config, Some(document), pending.clone())?;
            Ok(Self {
                requirements,
                snapshot: runtime.routing_snapshot(),
                router,
                pending,
                guarded: false,
            })
        }

        fn capture(&self) -> Result<Capture> {
            let capture = Capture::default();
            capture.initialize(Inputs::new(
                self.requirements.clone(),
                "execution",
                "owner",
                ModelOperation::Generation,
            )?)?;
            Ok(capture)
        }

        fn prompt() -> Prompt {
            Prompt {
                model: "fixture:strong".into(),
                system: Some(PRIVATE_MARKER.into()),
                system_provider_metadata: ProviderMetadata::new(),
                messages: vec![Message::text(Role::User, PRIVATE_MARKER)],
                tools: Vec::new(),
                params: GenerationParams::default(),
                response_format: None,
                tool_choice: None,
                stream: false,
            }
        }

        fn headers() -> http::HeaderMap {
            let mut headers = http::HeaderMap::new();
            headers.insert(
                http::header::AUTHORIZATION,
                http::HeaderValue::from_static("Bearer PRIVATE_CAPTURE_SENTINEL"),
            );
            headers
        }

        fn decision(&self) -> PolicyDecision {
            self.router
                .decision_for_routing(&Self::prompt(), &Self::headers(), None, self.guarded)
        }

        fn handoff(&self) -> Result<(EvalInvocation, PendingEvalDecision)> {
            let invocation = EvalInvocation::new("owner");
            let (selected, pending) = self.router.record_bound_policy_decision(
                ("execution", "execution"),
                &invocation,
                Self::prompt().model,
                None,
                self.decision(),
                &Self::headers(),
            );
            ensure!(
                selected.is_some(),
                "positive fixture must select a real target"
            );
            Ok((
                invocation,
                pending.context("missing direct decision handoff")?,
            ))
        }
    }

    #[test]
    fn guards_cannot_replace_model_only_capture() -> Result<()> {
        let fixture = Fixture::new(document()?)?;
        let capture = fixture.capture()?;
        let before = capture.snapshot().context("initial capture absent")?;
        let (_, pending) = fixture.handoff()?;
        ensure!(
            capture
                .policy("auto", true, Some(&pending), &fixture.snapshot)
                .is_err()
        );
        ensure!(capture.snapshot().as_deref() == Some(before.as_ref()));
        let mut raw = serde_json::to_value(document()?)?;
        raw["policies"]["auto"]["progress_guard"] = serde_json::json!({"escalation_tier":"strong"});
        ensure!(serde_json::from_value::<bitrouter_ai::policy::Document>(raw).is_err());
        Ok(())
    }

    #[test]
    fn frozen_name_digest_and_guard_reject_without_replacing_capture() -> Result<()> {
        {
            let guarded = false;
            let fixture = Fixture::new(document()?)?;
            let capture = fixture.capture()?;
            let before = capture.snapshot().context("missing initial capture")?;
            let (_, pending) = fixture.handoff()?;

            // Missing definition and an inverted guard fail even with no treatment.
            ensure!(
                capture
                    .policy("absent", guarded, None, &fixture.snapshot)
                    .is_err()
            );
            ensure!(
                capture
                    .policy("auto", !guarded, None, &fixture.snapshot)
                    .is_err()
            );
            let mut wrong_name = pending.clone();
            wrong_name.policy = "another-policy".into();
            ensure!(
                capture
                    .policy("auto", guarded, Some(&wrong_name), &fixture.snapshot)
                    .is_err()
            );
            let mut wrong_digest = pending.clone();
            wrong_digest.policy_digest = format!("sha256:{}", "0".repeat(64));
            ensure!(Some(wrong_digest.policy_digest.as_str()) != fixture.snapshot.digest());
            ensure!(
                capture
                    .policy("auto", guarded, Some(&wrong_digest), &fixture.snapshot)
                    .is_err()
            );
            ensure!(Arc::ptr_eq(
                &before,
                &capture.snapshot().context("rejection erased capture")?,
            ));

            // The same real snapshot is accepted with its actual name and guard.
            capture.policy("auto", guarded, Some(&pending), &fixture.snapshot)?;
            capture.ready()?;
            capture
                .snapshot()
                .context("missing ready capture")?
                .require_ready()?;
        }
        Ok(())
    }

    #[test]
    fn internally_consistent_new_lock_cannot_replace_admitted_policy_authority() -> Result<()> {
        let original_document = document()?;
        let mut replacement_document = original_document.clone();
        let definition = replacement_document
            .policies
            .get_mut("auto")
            .context("missing auto")?;
        definition.default_tier = definition
            .tiers
            .keys()
            .find(|tier| tier.as_str() != definition.default_tier.as_str())
            .cloned()
            .context("fixture needs a second valid tier")?;
        let original = Fixture::new(original_document)?;
        let replacement = Fixture::new(replacement_document)?;
        ensure!(original.snapshot.digest() != replacement.snapshot.digest());
        let (_, original_decision) = original.handoff()?;
        let (_, replacement_decision) = replacement.handoff()?;
        let capture = original.capture()?;
        let before = capture.snapshot().context("missing capture")?;

        // Both the replacement decision and its snapshot are individually valid;
        // the rejection must come from the original admitted requirements.
        ensure!(
            capture
                .policy(
                    "auto",
                    false,
                    Some(&replacement_decision),
                    &replacement.snapshot
                )
                .is_err()
        );
        ensure!(capture.snapshot().as_deref() == Some(before.as_ref()));
        capture.policy("auto", false, Some(&original_decision), &original.snapshot)?;
        capture.ready()?;
        let mut stored = capture
            .snapshot()
            .context("missing original")?
            .as_ref()
            .clone();
        let original_bytes = serde_json::to_vec(&stored)?;

        let replacement_capture = replacement.capture()?;
        replacement_capture.policy(
            "auto",
            false,
            Some(&replacement_decision),
            &replacement.snapshot,
        )?;
        replacement_capture.ready()?;
        let incoming = replacement_capture
            .snapshot()
            .context("missing replacement")?;
        incoming.require_ready()?;
        ensure!(stored.merge(&incoming).is_err());
        ensure!(serde_json::to_vec(&stored)? == original_bytes);
        Ok(())
    }

    #[test]
    fn normalized_policy_capture_excludes_untrusted_labels_and_request_content() -> Result<()> {
        let fixture = Fixture::new(document()?)?;
        let (_, mut pending) = fixture.handoff()?;
        pending.predicted_role = Some(PRIVATE_MARKER.into());
        pending.predicted_task_family = Some(PRIVATE_MARKER.into());
        pending.prediction_confidence_ppm = Some(u32::MAX);
        pending.task_family_confidence_ppm = Some(u32::MAX);
        pending.continuation_proposed_tier = Some("strong".into());
        pending.continuation_proposed_model = Some(format!("{PRIVATE_MARKER}\n"));
        pending.continuation_adjustment = Some(format!("{PRIVATE_MARKER}\0"));
        pending.predictor_contract_digest = Some(PRIVATE_MARKER.repeat(8));
        pending.prediction_confidence_kind = Some(PRIVATE_MARKER.repeat(4));
        let capture = fixture.capture()?;
        capture.policy("auto", false, Some(&pending), &fixture.snapshot)?;
        capture.ready()?;
        let ready = capture.snapshot().context("missing captured treatment")?;
        let Policy::Evaluated {
            treatment: Some(treatment),
            ..
        } = &ready.policy
        else {
            anyhow::bail!("fixture lost its selected treatment");
        };
        for name in ["predicted_role", "predicted_task_family"] {
            ensure!(treatment.attributes.get(name).map(String::as_str) == Some("unknown"));
        }
        for name in ["prediction_confidence_ppm", "task_family_confidence_ppm"] {
            ensure!(treatment.attributes.get(name).map(String::as_str) == Some("1000000"));
        }
        ensure!(
            treatment
                .attributes
                .get("continuation_proposed_tier")
                .map(String::as_str)
                == Some("strong")
        );
        for name in [
            "continuation_proposed_model",
            "continuation_adjustment",
            "predictor_contract_digest",
            "prediction_confidence_kind",
        ] {
            ensure!(!treatment.attributes.contains_key(name));
        }
        let encoded = serde_json::to_string(ready.as_ref())?;
        ensure!(!encoded.contains(PRIVATE_MARKER));
        ensure!(!encoded.contains("Bearer "));
        ensure!(treatment.decision.policy_digest == pending.policy_digest);
        ensure!(treatment.decision.selected_tier == pending.selected_tier);
        Ok(())
    }

    #[test]
    fn no_selected_tier_retains_evaluated_failure_without_ready_or_bypass() -> Result<()> {
        let fixture = Fixture::new(document()?)?;
        let invocation = EvalInvocation::new("owner");
        let mut decision = fixture.decision();
        decision.selected_tier = None;
        decision.selected_model = None;
        let (selected, pending) = fixture.router.record_bound_policy_decision(
            ("execution", "execution"),
            &invocation,
            Fixture::prompt().model,
            None,
            decision,
            &Fixture::headers(),
        );
        ensure!(selected.is_none() && pending.is_none());
        ensure!(fixture.pending.peek(&invocation, "owner").is_none());
        let capture = fixture.capture()?;
        capture.policy("auto", false, pending.as_ref(), &fixture.snapshot)?;
        let partial = capture.snapshot().context("missing partial policy")?;
        ensure!(matches!(
            &partial.policy,
            Policy::Evaluated {
                treatment: None,
                ..
            }
        ));
        ensure!(capture.ready().is_err());
        ensure!(capture.bypass(Bypass::UnboundRoute).is_err());
        ensure!(capture.snapshot().as_deref() == Some(partial.as_ref()));
        let terminal = capture.terminal()?.context("missing incomplete terminal")?;
        ensure!(terminal.phase == Phase::Terminal { route_ready: false });
        ensure!(terminal.policy == partial.policy);
        ensure!(terminal.require_ready().is_err());
        Ok(())
    }

    #[test]
    fn direct_handoff_survives_cache_removal_before_capture_and_source_drop() -> Result<()> {
        let fixture = Fixture::new(document()?)?;
        let (invocation, handed_off) = fixture.handoff()?;
        ensure!(fixture.pending.peek(&invocation, "owner").as_ref() == Some(&handed_off));
        ensure!(fixture.pending.remove(&invocation, "owner"));
        ensure!(fixture.pending.peek(&invocation, "owner").is_none());
        let capture = fixture.capture()?;
        capture.policy("auto", false, Some(&handed_off), &fixture.snapshot)?;
        capture.ready()?;
        let ready = capture.snapshot().context("missing retained handoff")?;
        let Policy::Evaluated {
            treatment: Some(treatment),
            ..
        } = &ready.policy
        else {
            anyhow::bail!("direct handoff disappeared with cache entry");
        };
        ensure!(treatment.decision.decision_id == handed_off.decision_id);
        ensure!(treatment.decision.policy_digest == handed_off.policy_digest);
        ensure!(treatment.decision.selected_tier == handed_off.selected_tier);
        ensure!(treatment.observed_at == handed_off.observed_at);
        let original_policy = serde_json::to_value(&ready.policy)?;
        drop(handed_off);
        drop(invocation);
        drop(fixture);
        let terminal = capture.terminal()?.context("missing terminal handoff")?;
        ensure!(serde_json::to_value(&terminal.policy)? == original_policy);
        ensure!(terminal.phase == Phase::Terminal { route_ready: true });
        Ok(())
    }

    #[test]
    fn cold_inputs_reject_unknown_contract_and_mismatched_digest_without_adoption() -> Result<()> {
        let fixture = Fixture::new(document()?)?;
        let (_, pending) = fixture.handoff()?;
        let capture = fixture.capture()?;
        capture.policy("auto", false, Some(&pending), &fixture.snapshot)?;
        capture.ready()?;
        let ready = capture.snapshot().context("missing ready fixture")?;
        let original_json = serde_json::to_value(ready.as_ref())?;
        let restored: Inputs = serde_json::from_value(original_json.clone())?;
        restored.require_ready()?;

        // Exercise cold adoption of a complete serialized Inputs envelope, not
        // just Requirements::validate in isolation (covered by existing tests).
        let mut future_format = original_json.clone();
        future_format["requirements"]["format"] = 2.into();
        let mut substituted_lock = original_json.clone();
        substituted_lock["requirements"]["source_digest"] =
            format!("sha256:{}", "0".repeat(64)).into();
        for json in [future_format, substituted_lock] {
            let rejected: Inputs = serde_json::from_value(json)?;
            ensure!(rejected.require_ready().is_err());
            let empty = Capture::default();
            ensure!(empty.initialize(rejected.clone()).is_err());
            ensure!(empty.snapshot().is_none());
            let mut stored = restored.clone();
            ensure!(stored.merge(&rejected).is_err());
            ensure!(serde_json::to_value(&stored)? == original_json);
        }
        let mut unknown_requirement = original_json.clone();
        unknown_requirement["requirements"]["policy_evaluation"] = "future_optional_sink".into();
        ensure!(serde_json::from_value::<Inputs>(unknown_requirement).is_err());
        let mut unknown_field = original_json.clone();
        unknown_field["requirements"]["ignore_missing_policy"] = true.into();
        ensure!(serde_json::from_value::<Inputs>(unknown_field).is_err());
        ensure!(
            serde_json::to_value(capture.snapshot().context("lost original")?.as_ref())?
                == original_json
        );
        Ok(())
    }
    #[test]
    fn dispatch_rejects_changed_effort_receipts_and_effective_model() -> Result<()> {
        use crate::gateway::caller::CallerContext;
        use crate::gateway::pipeline::{context::PipelineContext, types::PipelineRequest};
        use crate::gateway::routing::preparation::Receipt;
        use bitrouter_ai::types::ReasoningEffort;
        let context = |prompt: Prompt| {
            let mut request = PipelineRequest::new(
                prompt.model.clone(),
                CallerContext::new("key", "owner"),
                prompt,
            );
            request.request_id = "execution".into();
            PipelineContext::new(request)
        };
        let fixture = Fixture::new(document()?)?;
        let (_, pending) = fixture.handoff()?;
        let capture = fixture.capture()?;
        capture.policy("auto", false, Some(&pending), &fixture.snapshot)?;
        capture.ready()?;
        let selected = capture.snapshot().context("missing selected policy")?;
        let mut prompt = Fixture::prompt();
        prompt.params.reasoning_effort = pending.selected_effort;
        prompt.model = pending
            .route_measurement
            .as_ref()
            .context("measurement")?
            .candidates
            .iter()
            .find(|candidate| candidate.tier == pending.selected_tier)
            .context("selected model")?
            .model
            .clone();
        selected.check_routing(&context(prompt.clone()))?;
        prompt.params.reasoning_effort =
            Some(if pending.selected_effort == Some(ReasoningEffort::High) {
                ReasoningEffort::Low
            } else {
                ReasoningEffort::High
            });
        ensure!(selected.check_routing(&context(prompt)).is_err());

        let direct = Capture::default();
        direct.initialize(input()?)?;
        direct.bypass(Bypass::UnboundRoute)?;
        let mut ctx = context(Fixture::prompt());
        ctx.emit(Receipt {
            id: "semantic-fixture".into(),
            assessment: None,
            error: Some(
                crate::gateway::model::decision::types::DecisionError::invalid_request("fixture"),
            ),
            usage: None,
        });
        direct.route(routing(&ctx)?)?;
        direct.ready()?;
        let ready = direct.snapshot().context("missing direct route")?;
        ready.check_routing(&ctx)?;
        ensure!(
            ready.check_routing(&context(Fixture::prompt())).is_err(),
            "missing receipt was accepted"
        );
        let mut changed = Fixture::prompt();
        changed.params.reasoning_effort = pending.selected_effort;
        changed.model = "different-model".into();
        ensure!(
            selected.check_routing(&context(changed)).is_err(),
            "changed model was accepted"
        );
        let mut duplicate = ready.as_ref().clone();
        duplicate.routing.push(
            duplicate
                .routing
                .first()
                .context("missing receipt")?
                .clone(),
        );
        ensure!(duplicate.validate().is_err());
        Ok(())
    }
}
