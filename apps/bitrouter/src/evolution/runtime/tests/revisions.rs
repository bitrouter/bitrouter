use super::*;

#[tokio::test]
async fn assembled_revision_serves_pinned_routes_and_withdraws_inherited_adoption() -> Result<()> {
    let fixture = fixture(false).await?;
    let runtime = &fixture.assembled.evolution;
    runtime
        .register("local", definition("coding", "candidate"))
        .await?;
    let service = runtime.service("local")?;
    service.set_mode(EvolutionMode::Manual, None).await?;
    adopt_fixture(runtime).await?;
    let original = service
        .state()
        .await?
        .blocks
        .get("coding-block")
        .context("missing original")?
        .clone();
    let old = session(&fixture, "before-revision", "fixture").await?;
    let pipeline = fixture
        .assembled
        .app
        .language_model()
        .context("pipeline missing")?;
    pipeline
        .execute(request(&old, "old-request", "coding")?)
        .await?;
    let mut revised = original.definition.clone();
    revised.rules[0].baseline_route = "candidate".into();
    revised.rules[0].challenger_route = "coding".into();
    runtime
        .revise("local", revised, original.experiment_id.clone())
        .await?;
    let current = service
        .state()
        .await?
        .blocks
        .get("coding-block")
        .context("missing current")?
        .clone();
    let fresh = session(&fixture, "after-revision", "fixture").await?;
    pipeline
        .execute(request(&old, "old-after-revision", "coding")?)
        .await?;
    pipeline
        .execute(request(&fresh, "new-after-revision", "coding")?)
        .await?;
    let old_execution = runtime
        .executions(&old)
        .await?
        .pop()
        .context("missing old execution")?;
    assert_eq!(old_execution.dispatch_route, "candidate");
    assert_eq!(
        old_execution
            .settlement
            .context("missing old settlement")?
            .hops[0]
            .model,
        "cheap"
    );
    let enrollment = service
        .enrollment(&fresh)
        .await?
        .context("missing new enrollment")?;
    let assignment = enrollment
        .assignments
        .get("coding-block")
        .context("missing new assignment")?;
    assert_eq!(assignment.experiment_id, current.experiment_id);
    let new_execution = runtime
        .executions(&fresh)
        .await?
        .pop()
        .context("missing new execution")?;
    assert_eq!(
        new_execution
            .settlement
            .context("missing new settlement")?
            .hops[0]
            .model,
        if assignment.arm == Arm::Challenger {
            "strong"
        } else {
            "cheap"
        }
    );
    let archived = runtime
        .reconcile_experiment("local", "coding-block", Some(&original.experiment_id))
        .await?;
    assert!(archived.archived && archived.published);
    assert_eq!(
        archived.block_status,
        BlockStatus::RolledBack,
        "unresolved evidence does not support the arranged adoption"
    );
    for (identity, id) in [(&old, "withdrawn-old"), (&fresh, "withdrawn-new")] {
        pipeline.execute(request(identity, id, "coding")?).await?;
        let execution = runtime
            .executions(identity)
            .await?
            .pop()
            .context("missing withdrawn execution")?;
        assert_eq!(execution.dispatch_route, "coding");
        assert_eq!(
            execution
                .settlement
                .context("missing withdrawn settlement")?
                .hops[0]
                .model,
            "strong"
        );
    }
    assert_eq!(
        service
            .state()
            .await?
            .blocks
            .get("coding-block")
            .context("missing current")?
            .status,
        BlockStatus::RolledBack
    );
    Ok(())
}
