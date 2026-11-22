//! Catalog read/preparation identity over real HTTP sockets, without Core authority.

use super::*;
use bitrouter_ai::tenancy::{
    catalog::{CatalogCandidatesV1, CatalogSelectionRequestV1, CatalogSelectionV1},
    identity::InstallationId,
};
use bitrouter_sdk::error::Error;

fn reference(kind: &str, id: &str) -> Value {
    json!({"owner_kind":"operator_pool","owner_id":"platform","resource_kind":kind,
        "resource_id":id,"revision_id":"v1"})
}

fn descriptor() -> TestResult<CatalogCandidatesV1> {
    let credential = reference("credential", "slot");
    let profile = reference("policy", "platform-profile");
    let value = json!({
        "schema_version":1,
        "manifest":{
            "schema_version":1,"admission_mode":"profiled_v1","runtime_observation":null,
            "installation_id":"installation","tenant_id":"tenant","execution_revision":"catalog",
            "authority_revision":7,"route":reference("route","route"),"inbound_protocol":"chat_completions",
            "candidates":[{
                "candidate_index":0,"admission_profile":profile,"credential":credential,
                "target":{"provider_id":"opaque-provider","model_id":"opaque-model","account_id":"opaque-account",
                    "credential_id":"opaque-slot","credential_version":"v1"}
            }]
        },
        "sources":[{
            "candidate_index":0,"source_id":"provider/default/acme/model","provider":"provider",
            "account":null,"model":"native-model","outbound_protocol":"chat_completions"
        }],
        "templates":[{
            "template_id":"template","source_id":"provider/default/acme/model","provider":"provider",
            "account":null,"model":"native-model","outbound_protocol":"chat_completions",
            "credential_ref":credential,"platform_profile":profile,
            "byok_profile":reference("policy","byok-profile")
        }]
    });
    let result: CatalogCandidatesV1 = serde_json::from_value(value)?;
    result.validate()?;
    Ok(result)
}

fn instructions() -> TestResult<CatalogSelectionRequestV1> {
    let result: CatalogSelectionRequestV1 = serde_json::from_value(json!({
        "schema_version":1,"catalog_revision":"catalog","route_id":"route",
        "inbound_protocol":"chat_completions",
        "candidates":[{"template_id":"template","credential_ref":null}]
    }))?;
    result.validate()?;
    Ok(result)
}

fn prepared(input: &CatalogSelectionRequestV1) -> TestResult<CatalogSelectionV1> {
    let mut candidates = descriptor()?;
    candidates.manifest.execution_revision = "prepared".try_into()?;
    candidates.manifest.route.revision_id = "prepared-route".try_into()?;
    candidates.templates.clear();
    let result = CatalogSelectionV1 {
        selection: input.clone(),
        candidates,
    };
    result.validate()?;
    Ok(result)
}

async fn get(
    client: &TenancyClient,
    installation: &InstallationId,
) -> TestResult<CatalogCandidatesV1> {
    Ok(client
        .catalog_candidates(
            installation,
            &"tenant".try_into()?,
            "route",
            GenerationProtocol::ChatCompletions,
        )
        .await?)
}

#[tokio::test]
async fn catalog_get_and_prepare_send_exact_installation_on_http1_and_http2() -> TestResult {
    let installation: InstallationId = "installation".try_into()?;
    let input = instructions()?;
    for (version, wire) in versions() {
        let server = server::serve(
            wire,
            vec![
                Some(server::json(serde_json::to_value(descriptor()?)?)?),
                Some(server::json(serde_json::to_value(prepared(&input)?)?)?),
            ],
        )
        .await?;
        let client = control_client(&server, version)?;
        let catalog = get(&client, &installation).await?;
        assert_eq!(catalog.manifest.installation_id, installation);
        let result = client
            .prepare_catalog_selection(&installation, &"tenant".try_into()?, &input)
            .await?;
        assert_eq!(result.selection, input);
        assert_eq!(result.candidates.manifest.installation_id, installation);
        let records = server.records.lock().await;
        assert_eq!(records.len(), 2);
        for record in records.iter() {
            assert_eq!(record.version, wire);
            assert_eq!(
                record
                    .headers
                    .get_all("bitrouter-installation-id")
                    .iter()
                    .count(),
                1
            );
            assert_eq!(
                record
                    .headers
                    .get("bitrouter-installation-id")
                    .ok_or("installation header")?
                    .to_str()?,
                "installation"
            );
            assert_eq!(
                record
                    .headers
                    .get("authorization")
                    .ok_or("authorization header")?
                    .to_str()?,
                "Bearer operator-token"
            );
        }
        assert_eq!(records[0].method, "GET");
        assert_eq!(
            records[0].path,
            "/control/v1/tenants/tenant/catalog/routes/route/candidates?inbound_protocol=chat_completions"
        );
        assert!(records[0].body.is_empty());
        assert_eq!(records[1].method, "POST");
        assert_eq!(
            records[1].path,
            "/control/v1/tenants/tenant/catalog/selections"
        );
        assert_eq!(
            serde_json::from_slice::<Value>(&records[1].body)?,
            serde_json::to_value(&input)?
        );
    }
    Ok(())
}

#[tokio::test]
async fn catalog_get_and_prepare_reject_foreign_installation_without_retry_on_http1_and_http2()
-> TestResult {
    let installation: InstallationId = "installation".try_into()?;
    let input = instructions()?;
    for (version, wire) in versions() {
        let mut foreign_get = descriptor()?;
        foreign_get.manifest.installation_id = "foreign".try_into()?;
        foreign_get.validate()?;
        let mut foreign_prepare = prepared(&input)?;
        foreign_prepare.candidates.manifest.installation_id = "foreign".try_into()?;
        foreign_prepare.validate()?;
        let server = server::serve(
            wire,
            vec![
                Some(server::json(serde_json::to_value(foreign_get)?)?),
                Some(server::json(serde_json::to_value(foreign_prepare)?)?),
            ],
        )
        .await?;
        let client = control_client(&server, version)?;
        let result = client
            .catalog_candidates(
                &installation,
                &"tenant".try_into()?,
                "route",
                GenerationProtocol::ChatCompletions,
            )
            .await;
        assert!(matches!(result, Err(Error::Response(_))));
        let result = client
            .prepare_catalog_selection(&installation, &"tenant".try_into()?, &input)
            .await;
        assert!(matches!(result, Err(Error::Response(_))));
        let records = server.records.lock().await;
        assert_eq!(
            records.len(),
            2,
            "installation mismatch must not retry either method"
        );
        for record in records.iter() {
            assert_eq!(record.version, wire);
            assert_eq!(
                record
                    .headers
                    .get("bitrouter-installation-id")
                    .ok_or("installation header")?
                    .to_str()?,
                "installation"
            );
        }
    }
    Ok(())
}
