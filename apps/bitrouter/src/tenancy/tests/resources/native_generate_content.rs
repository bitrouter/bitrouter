//! Retained operation replay is independent of a newly disabled transport.
use super::*;
use bitrouter_ai::tenancy::resources_v3::ResourcePutV3;
use serde_json::json;

#[tokio::test]
async fn native_generate_content_committed_v3_replays_after_restart_without_adapter() -> Result<()>
{
    let mut fixture = keyed_fixture().await?;
    let slot_input = credential(&fixture, "native-replay-slot")?;
    let slot_result = put(&fixture, fixture.operator.clone(), slot_input.clone()).await?;
    let slot = reference(&slot_input, &slot_result)?;
    let profile_input: ResourcePut = serde_json::from_value(json!({
        "format":1,"operation_id":"native-replay-profile","installation_id":fixture.installation,
        "key":{"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"native-replay-profile"},
        "expected_revision":null,"definition":{"kind":"admission_profile","profile":{"schema_version":1,"rate":null,"health":null,"fallback":"ordered_terminal_rejections_v1"}}
    }))?;
    let profile_result = put(&fixture, fixture.operator.clone(), profile_input.clone()).await?;
    let profile = reference(&profile_input, &profile_result)?;
    let attachments = ["chat_completions","messages","responses","generate_content"].map(|protocol| json!({"inbound_protocol":protocol,"candidate_index":0,"credential":slot,"profile":profile}));
    let input: ResourcePutV3 = serde_json::from_value(json!({
        "format":3,"operation_id":"native-replay-route","installation_id":fixture.installation,
        "key":{"owner_kind":"tenant","owner_id":fixture.tenant,"resource_kind":"route","resource_id":"native-replay-route"},"expected_revision":null,
        "definition":{"kind":"native_profiled_route_v1","selector":"fixture:base","policy":null,
            "routing":{"providers":{"fixture":{"api_base":"http://127.0.0.1:1","api_protocol":[{"*":["ordered_generate_content","chat_completions","messages","responses"]}],"models":[{"id":"base"}]}}},
            "credentials":[{"provider":"fixture","account":"default","credential":slot}],"attachments":attachments}
    }))?;
    fixture.service.private_adapter = Some(Arc::new(
        crate::tenancy::execution::private_adapter::Peer::for_test(
            "http://127.0.0.1:1/generate-content".parse()?,
            [9; 32],
        )?,
    ));
    let created = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::ResourceV3(fixture.tenant.clone(), input.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    let revision = fixture.service.current().map_err(failure)?.revision;
    restart(&mut fixture).await?;
    ensure!(fixture.service.private_adapter.is_none());
    let replay = result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::ResourceV3(fixture.tenant.clone(), input.clone()),
            )
            .await
            .map_err(failure)?,
    )?;
    assert_eq!(
        serde_json::to_value(replay)?,
        serde_json::to_value(&created)?
    );
    let mut changed = input.clone();
    changed.expected_revision = created.resource_revision.clone();
    let rejected = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::ResourceV3(fixture.tenant.clone(), changed),
        )
        .await;
    ensure!(matches!(rejected,Err(error) if error.0.code==ErrorCode::OperationConflict));
    let mut fresh = input;
    fresh.operation_id = "disabled-new-v3".try_into()?;
    fresh.key.resource_id = "disabled-new-native-route".try_into()?;
    let rejected = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::ResourceV3(fixture.tenant.clone(), fresh),
        )
        .await;
    ensure!(matches!(rejected,Err(error) if error.0.code==ErrorCode::CapabilityMismatch));
    ensure!(fixture.service.current().map_err(failure)?.revision == revision);
    fixture.service.shutdown().await?;
    Ok(())
}
