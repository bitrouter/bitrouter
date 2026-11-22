mod history;

use super::*;
use serde_json::json;

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
async fn retained_profiled_protection_survives_revocation_and_restart() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let credential = credential(&fixture, "slot")?;
    let slot = reference(
        &credential,
        &put(&fixture, fixture.operator.clone(), credential.clone()).await?,
    )?;
    let pinned = super::history::seed(&fixture.service,&fixture.db,ResourceKey {
        owner_kind:OwnerKind::OperatorPool,owner_id:"pool".try_into()?,resource_kind:ResourceKind::Policy,resource_id:"profile".try_into()?,
    },None,json!({"kind":"admission_profile","profile":{"schema_version":1,"rate":{"rate_group_id":"shared","member_rpm":0},"health":null,"fallback":"stop"}})).await?;
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
    ensure!(
        !fixture
            .service
            .admission_snapshot_for_test()
            .await
            .map_err(failure)?
            .resource_admission
            .protected(&old)
    );
    let mut definition = serde_json::to_value(&legacy.definition)?;
    definition["kind"] = json!("profiled_route");
    definition["attachments"] =
        json!(["chat_completions","messages","responses"].map(|inbound_protocol|json!({
        "inbound_protocol":inbound_protocol,"candidate_index":0,"credential":slot,"profile":pinned,
    })));
    let protected_ref = super::history::seed(
        &fixture.service,
        &fixture.db,
        legacy.key.clone(),
        Some(old.revision_id.clone()),
        definition,
    )
    .await?;
    // Exact revocation does not remove the immutable protection record.
    for revoked in [protected_ref, pinned] {
        super::history::revoke(&fixture.service, &fixture.db, &revoked).await?;
    }
    restart(&mut fixture).await?;
    ensure!(
        fixture
            .service
            .admission_snapshot_for_test()
            .await
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
