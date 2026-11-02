use super::*;
use bitrouter_ai::tenancy::{
    admission::AdmissionAttachmentV1, identity::RevisionId, ordered::GenerationProtocol,
};
use serde_json::json;

fn profile(
    fixture: &Fixture,
    operation: &str,
    expected: Option<RevisionId>,
    rpm: u32,
) -> Result<ResourcePut> {
    let mut value = credential(fixture, operation)?;
    value.key = ResourceKey {
        owner_kind: OwnerKind::OperatorPool,
        owner_id: "pool".try_into()?,
        resource_kind: ResourceKind::Policy,
        resource_id: "profile".try_into()?,
    };
    value.expected_revision = expected;
    value.definition = Definition::AdmissionProfile {
        profile: serde_json::from_value(json!({"schema_version":1,
        "rate":{"rate_group_id":"shared","member_rpm":rpm},"health":null,"fallback":"stop"}))?,
    };
    Ok(value)
}

#[tokio::test]
async fn profile_cas_rotation_has_one_winner_preserves_rules_and_reloads_exact_head() -> Result<()>
{
    let mut fixture = keyed_fixture().await?;
    let original = profile(&fixture, "initial-profile", None, 0)?;
    let applied = put(&fixture, fixture.operator.clone(), original.clone()).await?;
    let pinned = reference(&original, &applied)?;
    let a = profile(&fixture, "profile-a", Some(pinned.revision_id.clone()), 7)?;
    let b = profile(&fixture, "profile-b", Some(pinned.revision_id.clone()), 11)?;
    let (a, b) = tokio::join!(
        fixture.service.execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), a)
        ),
        fixture.service.execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), b)
        ),
    );
    let mut applied = Vec::new();
    for response in [a, b] {
        match response.map_err(failure)? {
            StoredResponse::Operation(value) => applied.push(value),
            StoredResponse::Failure(error) => ensure!(error.code == ErrorCode::RevisionConflict),
            _ => anyhow::bail!("unexpected profile response"),
        }
    }
    ensure!(applied.len() == 1);
    let winner = applied[0].resource_revision.clone().context("winner")?;
    let mut invalid = profile(&fixture, "change-group", Some(winner.clone()), 17)?;
    let Definition::AdmissionProfile { profile: rules } = &mut invalid.definition else {
        anyhow::bail!("profile");
    };
    rules.rate.as_mut().context("rate")?.rate_group_id = "other".try_into()?;
    ensure!(
        matches!(fixture.service.execute(fixture.operator.clone(),Command::Resource(fixture.tenant.clone(),invalid)).await,
        Err(error) if error.0.code == ErrorCode::InvalidRequest)
    );
    restart(&mut fixture).await?;
    let state = fixture.service.current().map_err(failure)?;
    ensure!(state.resource_lifecycle.head(&pinned.key()) == Some(&winner));
    drop(state);
    fixture.service.shutdown().await?;
    // Re-reading an old immutable import must not rewind an RPM CAS winner.
    fixture
        .service
        .resources
        .provision_bytes(
            &fixture.db,
            &serde_json::to_vec(&json!({
                "format":1,"installation_id":fixture.installation,
                "resources":[{"reference":pinned,"definition":original.definition}],
            }))?,
        )
        .await?;
    restart(&mut fixture).await?;
    let state = fixture.service.current().map_err(failure)?;
    ensure!(state.resource_lifecycle.head(&pinned.key()) == Some(&winner));
    ensure!(
        state
            .resource_admission
            .profile(&pinned)
            .and_then(|profile| profile.rate.as_ref())
            .map(|rate| rate.member_rpm)
            == Some(0)
    );
    fixture.service.shutdown().await?;
    Ok(())
}

fn route_input(fixture: &Fixture, slot: &ResourceRef) -> Result<ResourcePut> {
    let mut route = credential(fixture, "legacy-route")?;
    route.key.resource_kind = ResourceKind::Route;
    route.key.resource_id = "route".try_into()?;
    route.definition = Definition::Route {
        routing: json!({"providers":{"mock":{"api_base":"http://127.0.0.1:1/v1",
            "api_protocol":[{"*":["chat_completions","messages","responses"]}],"models":[{"id":"model"}]}}}),
        selector: "mock:model".into(),
        policy: None,
        credentials: vec![bitrouter_ai::tenancy::resources::CredentialBinding {
            provider: "mock".into(),
            account: "default".into(),
            credential: slot.clone(),
        }],
    };
    Ok(route)
}

#[tokio::test]
async fn operator_only_profiled_route_protects_historical_configuration_after_restart() -> Result<()>
{
    let mut fixture = keyed_fixture().await?;
    let credential = credential(&fixture, "slot")?;
    let slot = reference(
        &credential,
        &put(&fixture, fixture.operator.clone(), credential.clone()).await?,
    )?;
    let initial = profile(&fixture, "profile", None, 0)?;
    let pinned = reference(
        &initial,
        &put(&fixture, fixture.operator.clone(), initial.clone()).await?,
    )?;
    let legacy = route_input(&fixture, &slot)?;
    let old = reference(
        &legacy,
        &put(&fixture, fixture.operator.clone(), legacy.clone()).await?,
    )?;
    let mut draft = fixture.configuration_input("old-config")?;
    draft.document.routes.push(old.clone());
    let retained = match fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Configuration(fixture.tenant.clone(), draft),
        )
        .await
        .map_err(failure)?
    {
        StoredResponse::Configuration(value) => value,
        _ => anyhow::bail!("configuration response"),
    };
    let mut protected = legacy.clone();
    protected.operation_id = "protected-route".try_into()?;
    protected.expected_revision = Some(old.revision_id.clone());
    let Definition::Route {
        routing,
        selector,
        policy,
        credentials,
    } = legacy.definition
    else {
        anyhow::bail!("legacy route");
    };
    protected.definition = Definition::ProfiledRoute {
        routing,
        selector,
        policy,
        credentials,
        attachments: [
            GenerationProtocol::ChatCompletions,
            GenerationProtocol::Messages,
            GenerationProtocol::Responses,
        ]
        .into_iter()
        .map(|inbound_protocol| AdmissionAttachmentV1 {
            inbound_protocol,
            candidate_index: 0,
            credential: slot.clone(),
            profile: pinned.clone(),
        })
        .collect(),
    };
    let issued = fixture.issue("tenant-editor").await?;
    let caller = fixture
        .service
        .authenticate(issued.secret.as_deref().context("secret")?)
        .map_err(failure)?;
    ensure!(
        matches!(fixture.service.execute(caller,Command::Resource(fixture.tenant.clone(),protected.clone())).await,
        Err(error) if error.0.code == ErrorCode::Forbidden)
    );
    ensure!(
        !fixture
            .service
            .current()
            .map_err(failure)?
            .resource_admission
            .protected(&old)
    );
    let publication = put(&fixture, fixture.operator.clone(), protected.clone()).await?;
    let protected_ref = reference(&protected, &publication)?;
    // Exact revocation does not remove the immutable protection record.
    for (index, revoked) in [protected_ref, pinned].into_iter().enumerate() {
        let revision = fixture.service.current().map_err(failure)?.revision;
        let response = fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::RevokeResource(
                    fixture.tenant.clone(),
                    ResourceRevoke {
                        format: 1,
                        operation_id: format!("revoke-{index}").try_into()?,
                        installation_id: fixture.installation.clone(),
                        reference: revoked,
                        expected_authority_revision: revision,
                    },
                ),
            )
            .await
            .map_err(failure)?;
        result(response)?;
    }
    restart(&mut fixture).await?;
    ensure!(
        fixture
            .service
            .current()
            .map_err(failure)?
            .resource_admission
            .protected(&old)
    );
    let activation = bitrouter_ai::tenancy::configuration::Activation {
        operation_id: "old-activation".try_into()?,
        installation_id: fixture.installation.clone(),
        revision_id: retained.revision_id.clone(),
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
        expected_active_revision: None,
    };
    let rejected = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Activate(fixture.tenant.clone(), activation),
        )
        .await
        .map_err(failure)?;
    ensure!(
        matches!(rejected,StoredResponse::Failure(error) if error.code == ErrorCode::CapabilityMismatch)
    );
    // Protection does not revoke archive/read authority over the retained draft.
    fixture
        .service
        .configuration(&fixture.operator, &fixture.tenant, &retained.revision_id)
        .await
        .map_err(failure)?;
    fixture.service.shutdown().await?;
    Ok(())
}
