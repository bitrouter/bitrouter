use super::*;
use crate::evolution::control::restoration::RestoreRequest;
use crate::evolution::service::revisions::RevisionRegistration;

#[tokio::test]
async fn operator_restore_is_durable_off_and_keeps_supported_parent_and_unrelated_block()
-> Result<()> {
    let (service, canonical) = setup().await?;
    service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    service
        .register(definition("b", "secondary"), "routes-b".into())
        .await?;
    service
        .set_mode(EvolutionMode::Manual, Some("judge".into()))
        .await?;
    let state = save(&service, |state| {
        adopt(state, "a")?;
        adopt(state, "b")?;
        let root = state
            .blocks
            .get("a")
            .context("root missing")?
            .experiment_id
            .clone();
        state.revise(
            next_definition(state, "primary-candidate", "third")?,
            "routes-v2".into(),
            &root,
            false,
        )?;
        adopt(state, "a")
    })
    .await?;
    let identity = new_session(&canonical, "before-operator-restore").await?;
    let before = service
        .select_with_bypass(
            &identity,
            "before-a",
            context("primary"),
            &live(&state),
            None,
        )
        .await?
        .context("intent missing")?;
    assert_eq!(before.selected_route, "third");
    let enrollment = serde_json::to_value(service.enrollment(&identity).await?)?;
    service.set_mode(EvolutionMode::Off, None).await?;
    let state = service.state().await?;
    let unrelated = serde_json::to_value(state.blocks.get("b"))?;
    let block = state.blocks.get("a").context("block missing")?;
    let request = RestoreRequest {
        block: "a".into(),
        expected_experiment: block.experiment_id.clone(),
        expected_revision: block.revision.clone(),
        reason: "Observed an unwanted behavior; withdraw this version.".into(),
    };
    let receipt = service.restore(&request).await?;
    assert_eq!(receipt.action, "operator_restore");
    assert!(receipt.evidence_digest.is_none());
    let restarted = EvolutionService::new(service.store.db.clone(), "owner")?;
    let after = restarted.state().await?;
    assert_eq!(after.mode, EvolutionMode::Off);
    assert_eq!(after.mode_epoch, state.mode_epoch);
    assert_eq!(after.trial_epoch, state.trial_epoch);
    assert_eq!(after.judge_model.as_deref(), Some("judge"));
    assert_eq!(serde_json::to_value(after.blocks.get("b"))?, unrelated);
    assert_eq!(
        serde_json::to_value(restarted.enrollment(&identity).await?)?,
        enrollment
    );
    let restored = restarted
        .select_with_bypass(
            &identity,
            "restored-a",
            context("primary"),
            &live(&after),
            None,
        )
        .await?
        .context("intent missing")?;
    assert_eq!(restored.selected_route, "primary-candidate");
    let retained = restarted
        .select_with_bypass(
            &identity,
            "retained-b",
            context("secondary"),
            &live(&after),
            None,
        )
        .await?
        .context("intent missing")?;
    assert_eq!(retained.selected_route, "secondary-candidate");
    assert_eq!(
        serde_json::to_value(restarted.restore(&request).await?)?,
        serde_json::to_value(&receipt)?
    );
    assert_eq!(restarted.state().await?.generation, after.generation);
    let mut conflicting = request.clone();
    conflicting.reason = "A different action against a stale version".into();
    assert!(restarted.restore(&conflicting).await.is_err());

    let next = save(&restarted, |state| {
        state.revise(
            next_definition(state, "primary-candidate", "fourth")?,
            "routes-v3".into(),
            &request.expected_experiment,
            false,
        )
    })
    .await?;
    // Retry acknowledges the old receipt without withdrawing the new trial.
    restarted.restore(&request).await?;
    assert_eq!(
        serde_json::to_value(restarted.state().await?)?,
        serde_json::to_value(next)?
    );
    Ok(())
}

#[tokio::test]
async fn operator_restore_fences_experiment_identity_even_with_an_unchanged_revision() -> Result<()>
{
    let (service, canonical) = setup().await?;
    let state = service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let block = state.blocks.get("a").context("block missing")?;
    let request = RestoreRequest {
        block: "a".into(),
        expected_experiment: block.experiment_id.clone(),
        expected_revision: block.revision.clone(),
        reason: "Stop this trial".into(),
    };
    let identity = new_session(&canonical, "exploring-withdrawal").await?;
    let original = service
        .select(
            &identity,
            "original-trial",
            context("primary"),
            &dependencies(),
        )
        .await?
        .context("intent missing")?;
    let revised = save(&service, |state| {
        state.revise(
            next_definition(state, "primary", "third")?,
            "routes-v2".into(),
            &request.expected_experiment,
            false,
        )
    })
    .await?;
    assert_eq!(
        revised.blocks.get("a").context("block missing")?.revision,
        request.expected_revision
    );
    assert!(service.restore(&request).await.is_err());
    assert_eq!(
        serde_json::to_value(service.state().await?)?,
        serde_json::to_value(&revised)?
    );
    let current = revised.blocks.get("a").context("block missing")?;
    let mut current_request = request;
    current_request.expected_experiment = current.experiment_id.clone();
    current_request.reason = "  ".into();
    assert!(service.restore(&current_request).await.is_err());
    current_request.reason = "Stop the current trial".into();
    service.restore(&current_request).await?;
    let after = service.state().await?;
    assert_eq!(
        after.blocks.get("a").context("block missing")?.status,
        BlockStatus::RolledBack
    );
    assert_eq!(
        service
            .intents(&identity)
            .await?
            .first()
            .context("intent missing")?
            .assignments,
        original.assignments
    );
    let fresh = new_session(&canonical, "after-exploration-withdrawal").await?;
    let route = service
        .select_with_bypass(
            &fresh,
            "new-baseline",
            context("primary"),
            &live(&after),
            None,
        )
        .await?
        .context("intent missing")?;
    assert_eq!(route.selected_route, "primary");
    assert!(route.assignments.is_empty());
    Ok(())
}

fn adopt(state: &mut ControlState, id: &str) -> Result<()> {
    let block = state.blocks.get_mut(id).context("missing block")?;
    block.batch.closed = true;
    let p = plan(
        &block.definition.bandit,
        &favorable_observations()?,
        "fixture-contract",
        None,
        41,
    )?;
    assert_eq!(p.recommendation, Recommendation::Promote);
    assert!(state.apply_plan(id, p, true)?);
    Ok(())
}

fn next_definition(
    state: &ControlState,
    baseline: &str,
    challenger: &str,
) -> Result<BlockDefinition> {
    let mut definition = state
        .blocks
        .get("a")
        .context("missing block")?
        .definition
        .clone();
    definition.rules[0].baseline_route = baseline.into();
    definition.rules[0].challenger_route = challenger.into();
    Ok(definition)
}

fn live(state: &ControlState) -> RoutingDependencies {
    RoutingDependencies {
        current: state
            .blocks
            .iter()
            .map(|(id, b)| (id.clone(), b.routing_config_digest.clone()))
            .collect(),
        archived: state
            .archived_experiments
            .iter()
            .map(|(id, a)| (id.clone(), a.block.routing_config_digest.clone()))
            .collect(),
        trial_ready: true,
        expected_control_generation: None,
    }
}

async fn save(
    service: &EvolutionService,
    change: impl FnOnce(&mut ControlState) -> Result<()>,
) -> Result<ControlState> {
    let tx = service.store.db.begin().await?;
    let (row, mut state): (_, ControlState) =
        service.store.lock(&tx, CONTROL_KIND, CONTROL_KEY).await?;
    change(&mut state)?;
    service.store.save(&tx, row, &state).await?;
    tx.commit().await?;
    Ok(state)
}

#[test]
fn revision_lineage_preserves_baselines_and_withdraws_only_descendants() -> Result<()> {
    let mut state = ControlState::default();
    state.set_mode(EvolutionMode::Manual, None)?;
    state.register(definition("a", "primary"), "routes-a".into())?;
    state.register(definition("b", "secondary"), "routes-b".into())?;
    adopt(&mut state, "a")?;
    let root = state.blocks.get("a").context("missing root")?.clone();
    let unrelated = serde_json::to_value(state.blocks.get("b"))?;
    state.revise(
        next_definition(&state, "primary-candidate", "third")?,
        "routes-v2".into(),
        &root.experiment_id,
        false,
    )?;
    let next = state.blocks.get("a").context("missing next")?;
    assert_eq!(next.revision, root.revision);
    assert_eq!(next.baseline_ancestry, vec![root.experiment_id.clone()]);
    assert!(next.plan.is_none() && next.batch.members.is_empty());
    assert_eq!(next.assigned_challenger_sessions, 0);
    let second = next.experiment_id.clone();
    adopt(&mut state, "a")?;
    state.revise(
        next_definition(&state, "third", "fourth")?,
        "routes-v3".into(),
        &second,
        false,
    )?;
    let third = state
        .blocks
        .get("a")
        .context("missing third")?
        .experiment_id
        .clone();

    // A late correction invalidates the second promotion; the first remains.
    let old = state
        .experiment("a", Some(&second))
        .context("missing archive")?;
    let hold = plan(
        &old.definition.bandit,
        &EffectiveObservations::default(),
        "fixture-contract",
        None,
        73,
    )?;
    assert_ne!(hold.recommendation, Recommendation::Promote);
    assert!(state.apply_experiment_plan("a", &second, hold, false)?);
    let current = state.blocks.get("a").context("missing current")?;
    assert_eq!(current.status, BlockStatus::RolledBack);
    assert_eq!(
        state.baseline_source(current)?.0.definition.rules[0].baseline_route,
        "primary-candidate"
    );
    assert_eq!(
        state
            .experiment("a", Some(&root.experiment_id))
            .context("missing root")?
            .status,
        BlockStatus::Adopted
    );
    assert_eq!(unrelated, serde_json::to_value(state.blocks.get("b"))?);
    state.revise(
        next_definition(&state, "primary-candidate", "fifth")?,
        "routes-v4".into(),
        &third,
        false,
    )?;
    assert_eq!(
        state
            .blocks
            .get("a")
            .context("missing current")?
            .baseline_ancestry,
        vec![root.experiment_id.clone()]
    );

    let root_block = state
        .experiment("a", Some(&root.experiment_id))
        .context("missing root")?;
    let hold = plan(
        &root_block.definition.bandit,
        &EffectiveObservations::default(),
        "fixture-contract",
        None,
        74,
    )?;
    state.apply_experiment_plan("a", &root.experiment_id, hold, false)?;
    let current = state.blocks.get("a").context("missing current")?;
    assert_eq!(
        state.baseline_source(current)?.0.definition.rules[0].baseline_route,
        "primary"
    );
    assert_eq!(current.status, BlockStatus::RolledBack);
    assert_eq!(unrelated, serde_json::to_value(state.blocks.get("b"))?);
    Ok(())
}

#[test]
fn retired_exploration_cannot_promote_and_revision_requires_reviewed_identity() -> Result<()> {
    let mut state = ControlState::default();
    state.set_mode(EvolutionMode::Manual, None)?;
    state.register(definition("a", "primary"), "routes-a".into())?;
    let root = state.blocks.get("a").context("missing root")?.clone();
    let mut changed = next_definition(&state, "primary", "third")?;
    changed.rules[0].selector = "other".into();
    assert!(
        state
            .revise(changed, "new".into(), &root.experiment_id, false)
            .is_err()
    );
    let wrong = next_definition(&state, "primary-candidate", "third")?;
    assert!(
        state
            .revise(wrong, "new".into(), &root.experiment_id, false)
            .is_err()
    );
    state.revise(
        next_definition(&state, "primary", "third")?,
        "new".into(),
        &root.experiment_id,
        false,
    )?;
    let latest = serde_json::to_value(state.blocks.get("a"))?;
    let p = plan(
        &root.definition.bandit,
        &favorable_observations()?,
        "fixture-contract",
        None,
        41,
    )?;
    assert_eq!(p.recommendation, Recommendation::Promote);
    assert!(state.apply_experiment_plan("a", &root.experiment_id, p, true)?);
    assert_eq!(
        state
            .experiment("a", Some(&root.experiment_id))
            .context("missing archive")?
            .status,
        BlockStatus::Exploring
    );
    assert_eq!(latest, serde_json::to_value(state.blocks.get("a"))?);
    assert_eq!(
        state.publications.last().map(|p| p.action.as_str()),
        Some("archived_evidence")
    );
    assert!(
        state
            .revise(
                next_definition(&state, "primary", "fourth")?,
                "newer".into(),
                &root.experiment_id,
                false
            )
            .is_err()
    );
    Ok(())
}

#[test]
fn a_second_ancestor_withdrawal_invalidates_dependencies_on_an_already_rolled_back_block()
-> Result<()> {
    let mut state = ControlState::default();
    state.set_mode(EvolutionMode::Manual, None)?;
    state.register(definition("a", "primary"), "routes-a".into())?;
    adopt(&mut state, "a")?;
    let root = state
        .blocks
        .get("a")
        .context("missing root")?
        .experiment_id
        .clone();
    state.revise(
        next_definition(&state, "primary-candidate", "third")?,
        "routes-v2".into(),
        &root,
        false,
    )?;
    adopt(&mut state, "a")?;
    let second = state
        .blocks
        .get("a")
        .context("missing second")?
        .experiment_id
        .clone();
    state.revise(
        next_definition(&state, "third", "fourth")?,
        "routes-v3".into(),
        &second,
        false,
    )?;
    let old = state
        .experiment("a", Some(&second))
        .context("missing second")?;
    let hold = plan(
        &old.definition.bandit,
        &EffectiveObservations::default(),
        "fixture-contract",
        None,
        42,
    )?;
    state.apply_experiment_plan("a", &second, hold, false)?;
    let current = state.blocks.get("a").context("missing current")?;
    assert_eq!(current.status, BlockStatus::RolledBack);
    let mut dependent = definition("b", "secondary");
    dependent
        .dependencies
        .insert("a".into(), current.revision.clone());
    state.register(dependent, "routes-b".into())?;
    assert!(state.dependencies_match(state.blocks.get("b").context("missing dependent")?));
    let old = state.experiment("a", Some(&root)).context("missing root")?;
    let hold = plan(
        &old.definition.bandit,
        &EffectiveObservations::default(),
        "fixture-contract",
        None,
        43,
    )?;
    state.apply_experiment_plan("a", &root, hold, false)?;
    let current = state.blocks.get("a").context("missing current")?;
    assert_eq!(
        state.baseline_source(current)?.0.definition.rules[0].baseline_route,
        "primary"
    );
    assert!(
        !state.dependencies_match(state.blocks.get("b").context("missing dependent")?),
        "a second baseline change must invalidate the dependent policy revision"
    );
    Ok(())
}

#[tokio::test]
async fn revision_pins_existing_sessions_and_registration_retries_preserve_history() -> Result<()> {
    let (service, canonical) = setup().await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let empty = new_session(&canonical, "before-any-block").await?;
    service
        .select(&empty, "empty", context("primary"), &BTreeMap::new())
        .await?;
    let initial = definition("a", "primary");
    service.register(initial.clone(), "routes-a".into()).await?;
    // Pin a pre-trial baseline deterministically by admitting with mode off.
    service.set_mode(EvolutionMode::Off, None).await?;
    let baseline = new_session(&canonical, "old-baseline").await?;
    service
        .select(&baseline, "baseline", context("primary"), &dependencies())
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let state = save(&service, |state| adopt(state, "a")).await?;
    let root = state
        .blocks
        .get("a")
        .context("missing root")?
        .experiment_id
        .clone();
    let deployed = new_session(&canonical, "old-adoption").await?;
    let old = service
        .select(&deployed, "adopted", context("primary"), &dependencies())
        .await?
        .context("missing intent")?;
    assert_eq!(old.selected_route, "primary-candidate");
    let revised = next_definition(&state, "primary-candidate", "third")?;
    let expected = (state.generation, state.mode_epoch);
    // Admission changes sequence but not the generation fence.
    let make_request = || RevisionRegistration {
        definition: revised.clone(),
        routing_digest: "routes-v2".into(),
        predecessor: root.clone(),
        reset_to_configured: false,
        expected_control: expected,
    };
    let state = service.revise_checked(make_request(), || Ok(())).await?;
    let current_id = state
        .blocks
        .get("a")
        .context("missing current")?
        .experiment_id
        .clone();
    let restarted = EvolutionService::new(service.store.db.clone(), "owner")?;
    let dependencies = live(&state);
    for (identity, request, expected_route) in [
        (&empty, "still-empty", "primary"),
        (&baseline, "still-baseline", "primary"),
        (&deployed, "still-adopted", "primary-candidate"),
    ] {
        let result = restarted
            .select_with_bypass(identity, request, context("primary"), &dependencies, None)
            .await?
            .context("missing intent")?;
        assert_eq!(result.selected_route, expected_route);
    }
    // Older persisted enrollments lack the explicit experiment snapshot.
    for (identity, request, expected_route) in [
        (&baseline, "legacy-baseline", "primary"),
        (&deployed, "legacy-adopted", "primary-candidate"),
    ] {
        let key = identity.key()?;
        let (revision, _): (_, SessionEnrollment) = restarted
            .store
            .get(SESSION_KIND, &key)
            .await?
            .context("missing enrollment")?;
        restarted
            .store
            .update(
                SESSION_KIND,
                &key,
                revision,
                |enrollment: &mut SessionEnrollment| {
                    enrollment.block_experiments = None;
                    Ok(())
                },
            )
            .await?;
        let result = restarted
            .select_with_bypass(identity, request, context("primary"), &dependencies, None)
            .await?
            .context("missing legacy intent")?;
        assert_eq!(result.selected_route, expected_route);
    }
    // Inherited baseline serving does not make incomplete capture trial-ready.
    let incomplete = new_session(&canonical, "incomplete-capture").await?;
    let mut incomplete_dependencies = dependencies.clone();
    incomplete_dependencies.trial_ready = false;
    let incomplete_intent = restarted
        .select_with_bypass(
            &incomplete,
            "without-coverage",
            context("primary"),
            &incomplete_dependencies,
            None,
        )
        .await?
        .context("missing baseline intent")?;
    assert_eq!(incomplete_intent.selected_route, "primary-candidate");
    assert!(incomplete_intent.assignments.is_empty());
    let later = restarted
        .select_with_bypass(
            &incomplete,
            "with-coverage",
            context("primary"),
            &dependencies,
            None,
        )
        .await?
        .context("missing later intent")?;
    assert!(later.assignments.is_empty());
    let fresh = new_session(&canonical, "new-version").await?;
    restarted
        .select_with_bypass(&fresh, "fresh", context("primary"), &dependencies, None)
        .await?;
    let enrollment = restarted
        .enrollment(&fresh)
        .await?
        .context("missing enrollment")?;
    assert_eq!(
        enrollment
            .block_experiments
            .as_ref()
            .and_then(|ids| ids.get("a")),
        Some(&current_id)
    );
    assert_eq!(
        enrollment.assignments.get("a").map(|a| &a.experiment_id),
        Some(&current_id)
    );
    restarted.set_mode(EvolutionMode::Off, None).await?;
    let raced = new_session(&canonical, "raced-control").await?;
    let mut stale = dependencies.clone();
    stale.expected_control_generation = Some(state.generation);
    assert!(
        restarted
            .select_with_bypass(&raced, "stale-control", context("primary"), &stale, None)
            .await
            .is_err()
    );
    assert!(restarted.enrollment(&raced).await?.is_none());
    let before_retry = restarted.state().await?;
    let replay = restarted
        .revise_checked(make_request(), || anyhow::bail!("must not run on retry"))
        .await?;
    assert_eq!(
        serde_json::to_value(&replay)?,
        serde_json::to_value(&before_retry)?
    );
    let original_replay = restarted
        .register_checked(initial, "routes-a".into(), Some((0, 0)), || {
            anyhow::bail!("must not run on retry")
        })
        .await?;
    assert_eq!(
        original_replay
            .blocks
            .get("a")
            .context("missing current")?
            .experiment_id,
        current_id
    );
    assert_eq!(
        original_replay.original_experiment("a")?.experiment_id,
        root
    );
    Ok(())
}

#[tokio::test]
async fn inherited_withdrawal_routes_to_the_last_supported_baseline() -> Result<()> {
    let (service, canonical) = setup().await?;
    service
        .register(definition("a", "primary"), "routes-a".into())
        .await?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    let state = save(&service, |state| {
        adopt(state, "a")?;
        let root = state
            .blocks
            .get("a")
            .context("missing root")?
            .experiment_id
            .clone();
        state.revise(
            next_definition(state, "primary-candidate", "third")?,
            "routes-v2".into(),
            &root,
            false,
        )?;
        adopt(state, "a")?;
        let second = state
            .blocks
            .get("a")
            .context("missing second")?
            .experiment_id
            .clone();
        state.revise(
            next_definition(state, "third", "fourth")?,
            "routes-v3".into(),
            &second,
            false,
        )
    })
    .await?;
    let second = state
        .blocks
        .get("a")
        .context("missing current")?
        .predecessor_experiment_id
        .clone()
        .context("missing parent")?;
    service.set_mode(EvolutionMode::Off, None).await?;
    let identity = new_session(&canonical, "third-baseline").await?;
    let before = service
        .select_with_bypass(&identity, "before", context("primary"), &live(&state), None)
        .await?
        .context("missing intent")?;
    assert_eq!(before.selected_route, "third");
    service.set_mode(EvolutionMode::Manual, None).await?;
    let state = save(&service, |state| {
        let ancestor = state
            .experiment("a", Some(&second))
            .context("missing second")?;
        let p = plan(
            &ancestor.definition.bandit,
            &EffectiveObservations::default(),
            "fixture-contract",
            None,
            4,
        )?;
        state.apply_experiment_plan("a", &second, p, false)?;
        Ok(())
    })
    .await?;
    let after = service
        .select_with_bypass(&identity, "after", context("primary"), &live(&state), None)
        .await?
        .context("missing intent")?;
    assert_eq!(after.selected_route, "primary-candidate");
    assert_eq!(after.reason, "block_rolled_back");
    // Missing validation of the inherited fallback never dispatches it.
    let mut changed = live(&state);
    changed.archived.remove(&second);
    let invalid = service
        .select_with_bypass(&identity, "invalid", context("primary"), &changed, None)
        .await?
        .context("missing intent")?;
    assert_eq!(invalid.selected_route, "primary");
    assert_eq!(invalid.reason, "routing_dependency_changed");
    Ok(())
}
