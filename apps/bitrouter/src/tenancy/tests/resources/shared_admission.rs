use super::*;
use bitrouter_ai::tenancy::{
    admission_v2::AdmissionProfileV2,
    identity::RevisionId,
    resources_v2::{DefinitionV2, ResourcePutV2},
};
use serde_json::json;

fn input(fixture: &Fixture, name: &str, operation: &str, rpm: u32) -> Result<ResourcePutV2> {
    Ok(ResourcePutV2 {
        format: 2,
        operation_id: operation.try_into()?,
        installation_id: fixture.installation.clone(),
        key: ResourceKey {
            owner_kind: OwnerKind::OperatorPool,
            owner_id: "pool".try_into()?,
            resource_kind: ResourceKind::Policy,
            resource_id: name.try_into()?,
        },
        expected_revision: None,
        definition: DefinitionV2::AdmissionProfileV2 {
            profile: serde_json::from_value(json!({"schema_version":2,
                "rate":{"rate_group_id":"window","member_rpm":rpm},
                "health":{"rules":"availability_v1","failure_threshold":3,
                    "transient_cooldown_seconds":10,"persistent_cooldown_seconds":30,"probe_lease_seconds":5},
                "health_group_id":"shared-health","fallback":"ordered_terminal_rejections_v1"}))?,
        },
    })
}

fn rules(input: &mut ResourcePutV2) -> &mut AdmissionProfileV2 {
    let DefinitionV2::AdmissionProfileV2 { profile } = &mut input.definition;
    profile
}

fn local_input(fixture: &Fixture, operation: &str) -> Result<ResourcePut> {
    let mut local = credential(fixture, operation)?;
    local.key = input(fixture, "profile", operation, 1)?.key;
    local.definition = Definition::AdmissionProfile {
        profile: serde_json::from_value(json!({"schema_version":1,
            "rate":{"rate_group_id":"window","member_rpm":1},
            "health":null,"fallback":"stop"}))?,
    };
    Ok(local)
}

fn pinned(input: &ResourcePutV2, revision: RevisionId) -> ResourceRef {
    ResourceRef {
        owner_kind: input.key.owner_kind,
        owner_id: input.key.owner_id.clone(),
        resource_kind: input.key.resource_kind,
        resource_id: input.key.resource_id.clone(),
        revision_id: revision,
    }
}

async fn put_v2(fixture: &Fixture, input: ResourcePutV2) -> Result<OperationResult> {
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::ResourceV2(fixture.tenant.clone(), input),
            )
            .await
            .map_err(failure)?,
    )
}

async fn rejected(
    fixture: &Fixture,
    caller: Caller,
    input: ResourcePutV2,
    code: ErrorCode,
) -> Result<()> {
    let before = fixture.service.current().map_err(failure)?;
    let revision = before.revision;
    let count = before.resource_lifecycle.len();
    let head = before.resource_lifecycle.head(&input.key).cloned();
    let key = input.key.clone();
    drop(before);
    let response = fixture
        .service
        .execute(caller, Command::ResourceV2(fixture.tenant.clone(), input))
        .await;
    match response {
        Err(error) => ensure!(error.0.code == code),
        Ok(StoredResponse::Failure(error)) => ensure!(error.code == code),
        _ => anyhow::bail!("unexpected accepted profile"),
    }
    let after = fixture.service.current().map_err(failure)?;
    ensure!(after.revision == revision && after.resource_lifecycle.len() == count);
    ensure!(after.resource_lifecycle.head(&key) == head.as_ref());
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT COUNT(*) AS count FROM tenancy_resource_revisions".to_owned(),
        ))
        .await?
        .context("catalog count")?;
    ensure!(usize::try_from(row.try_get::<i64>("", "count")?)? == count);
    Ok(())
}

#[tokio::test]
async fn conflicting_first_members_have_one_winner_and_never_publish_loser_state() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let first = input(&fixture, "first", "first", 1)?;
    let mut second = input(&fixture, "second", "second", 3)?;
    rules(&mut second)
        .health
        .as_mut()
        .context("health")?
        .failure_threshold = 4;
    let (a, b) = tokio::join!(
        fixture.service.execute(
            fixture.operator.clone(),
            Command::ResourceV2(fixture.tenant.clone(), first.clone())
        ),
        fixture.service.execute(
            fixture.operator.clone(),
            Command::ResourceV2(fixture.tenant.clone(), second.clone())
        ),
    );
    let mut winners = Vec::new();
    let mut losers = Vec::new();
    for (command, response) in [(first, a), (second, b)] {
        match response {
            Ok(StoredResponse::Operation(result)) => winners.push((command, result)),
            Ok(StoredResponse::Failure(error)) => {
                ensure!(error.code == ErrorCode::InvalidRequest);
                losers.push(command);
            }
            Err(error) => {
                ensure!(error.0.code == ErrorCode::InvalidRequest);
                losers.push(command);
            }
            _ => anyhow::bail!("unexpected group race response"),
        }
    }
    ensure!(winners.len() == 1 && losers.len() == 1);
    restart(&mut fixture).await?;
    let (winner, applied) = winners.pop().context("winner")?;
    let mut loser = losers.pop().context("loser")?;
    let state = fixture.service.current().map_err(failure)?;
    ensure!(state.resource_lifecycle.len() == 1);
    ensure!(state.resource_lifecycle.head(&winner.key) == applied.resource_revision.as_ref());
    ensure!(state.resource_lifecycle.head(&loser.key).is_none());
    drop(state);
    // The rejected candidate did not install its conflicting group rules.
    loser.operation_id = "compatible-after-race".try_into()?;
    loser.definition = winner.definition.clone();
    rules(&mut loser).rate.as_mut().context("rate")?.member_rpm = 9;
    put_v2(&fixture, loser).await?;
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn every_group_rule_is_immutable_across_rotation_all_revocation_and_restart() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let original = input(&fixture, "original", "original", 1)?;
    let created = put_v2(&fixture, original.clone()).await?;
    let old = pinned(&original, created.resource_revision.context("revision")?);
    let mut rotate = original.clone();
    rotate.operation_id = "rotate".try_into()?;
    rotate.expected_revision = Some(old.revision_id.clone());
    rules(&mut rotate).rate.as_mut().context("rate")?.member_rpm = 0;
    let rotated = put_v2(&fixture, rotate.clone()).await?;
    let current = pinned(&rotate, rotated.resource_revision.context("revision")?);
    for (n, reference) in [old.clone(), current.clone()].into_iter().enumerate() {
        let revision = fixture.service.current().map_err(failure)?.revision;
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::RevokeResource(
                        fixture.tenant.clone(),
                        ResourceRevoke {
                            format: 1,
                            operation_id: format!("revoke-{n}").try_into()?,
                            installation_id: fixture.installation.clone(),
                            reference,
                            expected_authority_revision: revision,
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
    }
    restart(&mut fixture).await?;
    for n in 0..4 {
        let mut different = input(
            &fixture,
            &format!("different-{n}"),
            &format!("different-{n}"),
            3,
        )?;
        let health = rules(&mut different).health.as_mut().context("health")?;
        match n {
            0 => health.failure_threshold += 1,
            1 => health.transient_cooldown_seconds += 1,
            2 => health.persistent_cooldown_seconds += 1,
            _ => health.probe_lease_seconds += 1,
        }
        rejected(
            &fixture,
            fixture.operator.clone(),
            different,
            ErrorCode::InvalidRequest,
        )
        .await?;
    }
    let sibling = input(&fixture, "new-member", "new-member", 3)?;
    put_v2(&fixture, sibling).await?;
    // Only the complete health rule is shared across different member keys.
    let mut independent = input(&fixture, "independent-rate", "independent-rate", 9)?;
    rules(&mut independent)
        .rate
        .as_mut()
        .context("rate")?
        .rate_group_id = "different-window".try_into()?;
    rules(&mut independent).fallback = bitrouter_ai::tenancy::admission::FallbackRule::Stop;
    put_v2(&fixture, independent).await?;
    let mut observed = input(&fixture, "observed-only", "observed-only", 9)?;
    rules(&mut observed).rate = None;
    rules(&mut observed).observe_rate_group_id = Some("window".try_into()?);
    put_v2(&fixture, observed).await?;
    let mut foreign = input(&fixture, "different-pool", "different-pool", 3)?;
    foreign.key.owner_id = "other-pool".try_into()?;
    rules(&mut foreign)
        .health
        .as_mut()
        .context("health")?
        .failure_threshold = 8;
    put_v2(&fixture, foreign).await?;
    let mut move_group = rotate.clone();
    move_group.operation_id = "move-group".try_into()?;
    move_group.expected_revision = Some(current.revision_id.clone());
    rules(&mut move_group).health_group_id = Some("replacement".try_into()?);
    rejected(
        &fixture,
        fixture.operator.clone(),
        move_group,
        ErrorCode::InvalidRequest,
    )
    .await?;
    let state = fixture.service.current().map_err(failure)?;
    ensure!(state.resource_lifecycle.revoked(&old) && state.resource_lifecycle.revoked(&current));
    ensure!(state.resource_lifecycle.head(&original.key) == Some(&current.revision_id));
    drop(state);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn v2_replay_operator_authority_and_cross_version_operation_identity() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let original = input(&fixture, "profile", "create-v2", 1)?;
    let created = put_v2(&fixture, original.clone()).await?;
    let replay = put_v2(&fixture, original.clone()).await?;
    ensure!(created.resource_revision == replay.resource_revision);
    let issued = fixture.issue("tenant-editor").await?;
    let caller = fixture
        .service
        .authenticate(issued.secret.as_deref().context("secret")?)
        .await
        .map_err(failure)?;
    rejected(
        &fixture,
        caller,
        input(&fixture, "forbidden", "forbidden", 3)?,
        ErrorCode::Forbidden,
    )
    .await?;
    let mut v1 = local_input(&fixture, "create-v2")?;
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), Command::Resource(fixture.tenant.clone(), v1.clone())).await, Err(error) if error.0.code == ErrorCode::OperationConflict)
    );
    // A new operation still cannot reinterpret the retained member's family.
    v1.operation_id = "change-to-v1".try_into()?;
    v1.expected_revision = created.resource_revision.clone();
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(), Command::Resource(fixture.tenant.clone(), v1)).await, Err(error) if error.0.code == ErrorCode::InvalidRequest)
    );
    let mut local = local_input(&fixture, "local-v1")?;
    local.key.resource_id = "local".try_into()?;
    let local_revision = put(&fixture, fixture.operator.clone(), local)
        .await?
        .resource_revision;
    let mut changed = input(&fixture, "local", "change-to-v2", 1)?;
    changed.expected_revision = local_revision;
    rejected(
        &fixture,
        fixture.operator.clone(),
        changed,
        ErrorCode::InvalidRequest,
    )
    .await?;
    restart(&mut fixture).await?;
    ensure!(put_v2(&fixture, original).await?.resource_revision == created.resource_revision);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn revoked_policy_lock_and_shared_profile_keys_cannot_change_family_after_restart()
-> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let original = input(&fixture, "shared", "create-shared", 1)?;
    let created = put_v2(&fixture, original.clone()).await?;
    let shared_ref = pinned(
        &original,
        created.resource_revision.context("shared revision")?,
    );
    let mut document = crate::policy_lock::PolicyLock::default();
    document.policies.insert(
        "selected".into(),
        crate::policy_lock::PolicyDefinition {
            tiers: [(
                "only".into(),
                bitrouter_ai::routing::PolicyModelTarget::model_only("fixture:model".into()),
            )]
            .into(),
            default_tier: Some("only".into()),
            ..Default::default()
        },
    );
    let mut policy = local_input(&fixture, "create-policy")?;
    policy.key.resource_id = "policy-lock".try_into()?;
    policy.definition = Definition::Policy {
        document: serde_json::to_value(document)?,
        name: "selected".into(),
        dependencies: Vec::new(),
    };
    // A successful independent put proves the policy fixture is itself valid.
    let created = put(&fixture, fixture.operator.clone(), policy.clone()).await?;
    let mut policy_ref = shared_ref.clone();
    policy_ref.resource_id = policy.key.resource_id.clone();
    policy_ref.revision_id = created.resource_revision.context("policy revision")?;
    for (n, reference) in [shared_ref.clone(), policy_ref.clone()]
        .into_iter()
        .enumerate()
    {
        let expected_authority_revision = fixture.service.current().map_err(failure)?.revision;
        result(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::RevokeResource(
                        fixture.tenant.clone(),
                        ResourceRevoke {
                            format: 1,
                            operation_id: format!("revoke-family-{n}").try_into()?,
                            installation_id: fixture.installation.clone(),
                            reference,
                            expected_authority_revision,
                        },
                    ),
                )
                .await
                .map_err(failure)?,
        )?;
    }
    restart(&mut fixture).await?;
    let mut replacement = input(&fixture, "policy-lock", "policy-to-shared", 1)?;
    replacement.expected_revision = Some(policy_ref.revision_id.clone());
    rejected(
        &fixture,
        fixture.operator.clone(),
        replacement,
        ErrorCode::InvalidRequest,
    )
    .await?;
    policy.operation_id = "shared-to-policy".try_into()?;
    policy.key = shared_ref.key();
    policy.expected_revision = Some(shared_ref.revision_id.clone());
    let before = fixture.service.current().map_err(failure)?.revision;
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(),Command::Resource(fixture.tenant.clone(),policy)).await,Err(error) if error.0.code==ErrorCode::InvalidRequest)
    );
    let state = fixture.service.current().map_err(failure)?;
    ensure!(state.revision == before && state.resource_lifecycle.len() == 2);
    for reference in [shared_ref, policy_ref] {
        ensure!(state.resource_lifecycle.head(&reference.key()) == Some(&reference.revision_id));
        ensure!(state.resource_lifecycle.revoked(&reference));
    }
    drop(state);
    fixture.service.shutdown().await?;
    Ok(())
}
