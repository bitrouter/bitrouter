//! Current provisioning uses real HTTP sockets and never silently downgrades.
#[path = "transport_protocol/server.rs"]
mod server;

use axum::{body::Body, http::Version, response::Response};
use bitrouter_ai::tenancy::resources::{Definition, ResourcePut};
use bitrouter_sdk::{
    error::Error,
    gateway::HttpVersion,
    tenancy::{TenancyClient, TenancyOptions},
};
use serde_json::{Value, json};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TOKEN: &str = "fixture-tenancy-token-0123456789abcdef";

fn capabilities() -> Value {
    json!({"format":1,"installation_id":"installation","provisioning_enabled":true,
        "max_command_bytes":1048576,"tenant_routes_require_approved_origins":true,
        "generation_protocols":["chat_completions","messages","responses","generate_content"]})
}

fn input() -> TestResult<ResourcePut> {
    Ok(serde_json::from_value(
        json!({"format":1,"operation_id":"operation","installation_id":"installation",
        "key":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"route","resource_id":"native-route"},
        "expected_revision":null,"definition":{"kind":"profiled_route","routing":{"providers":{}},"selector":"provider/model","policy":null,
        "credentials":[{"provider":"provider","account":"default","credential":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential","resource_id":"key","revision_id":"revision"}}],
        "attachments":[{"inbound_protocol":"generate_content","candidate_index":0,
            "credential":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential","resource_id":"key","revision_id":"revision"},
            "profile":{"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"profile","revision_id":"revision"}}]}}),
    )?)
}

fn applied() -> Value {
    json!({"operation_id":"operation","status":"applied","authority_revision":1,
        "resource_revision":"revision","resource_id":"native-route","error":null})
}

#[tokio::test]
async fn discovery_and_put_bind_installation_on_one_current_route() -> TestResult {
    for (transport, version) in [
        (HttpVersion::Http1Only, Version::HTTP_11),
        #[cfg(feature = "http2")]
        (HttpVersion::Http2PriorKnowledge, Version::HTTP_2),
    ] {
        let server = server::serve(
            version,
            vec![
                Some(server::json(capabilities())?),
                Some(server::json(applied())?),
                Some(server::json(capabilities())?),
            ],
        )
        .await?;
        let client = TenancyClient::with_options(
            &server.endpoint,
            TOKEN,
            TenancyOptions {
                http_version: transport,
                ..Default::default()
            },
        )?;
        let request = input()?;
        let caps = client
            .resource_capabilities(&request.installation_id)
            .await?;
        assert!(caps.provisioning_enabled);
        assert_eq!(
            caps.generation_protocols[3],
            bitrouter_ai::tenancy::ordered::GenerationProtocol::GenerateContent
        );
        let result = client.put_resource(&"tenant".try_into()?, &request).await?;
        assert_eq!(
            result.resource_revision.as_ref().map(|r| r.as_str()),
            Some("revision")
        );
        assert_eq!(
            client
                .resource_capabilities(&request.installation_id)
                .await?
                .format,
            1
        );
        let records = server.records.lock().await;
        assert_eq!(records.len(), 3);
        for (record, (method, path)) in records.iter().zip([
            ("GET", "/control/v1/tenancy/resource-capabilities"),
            ("POST", "/control/v1/tenants/tenant/resources"),
            ("GET", "/control/v1/tenancy/resource-capabilities"),
        ]) {
            assert_eq!(record.method, method);
            assert_eq!(record.path, path);
            assert_eq!(record.version, version);
            assert_eq!(record.connection, records[0].connection);
            assert_eq!(record.headers["Bitrouter-Installation-Id"], "installation");
            assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
        }
        assert_eq!(records[1].body, serde_json::to_vec(&request)?);
        assert!(records[0].body.is_empty() && records[2].body.is_empty());
    }
    Ok(())
}

#[tokio::test]
async fn unsupported_malformed_or_wrong_installation_discovery_never_writes_or_falls_back()
-> TestResult {
    for case in [
        "not_found",
        "missing_protocols",
        "foreign",
        "protocols",
        "fraction",
        "duplicate",
        "disabled",
    ] {
        let mut value = capabilities();
        match case {
            "missing_protocols" => {
                value["format"] = json!(1);
                value
                    .as_object_mut()
                    .ok_or("capabilities")?
                    .remove("generation_protocols");
            }
            "foreign" => value["installation_id"] = json!("foreign"),
            "protocols" => {
                value["generation_protocols"] = json!(["chat_completions", "messages", "responses"])
            }
            "disabled" => value["provisioning_enabled"] = json!(false),
            _ => {}
        }
        let mut raw = value.to_string();
        if case == "fraction" {
            raw = raw.replace("\"format\":1", "\"format\":1.00000000000000001");
        }
        if case == "duplicate" {
            raw = raw.replace("\"format\":1", "\"format\":1,\"format\":1");
        }
        let response = Response::builder()
            .status(if case == "not_found" { 404 } else { 200 })
            .header("content-type", "application/json")
            .body(Body::from(raw))?;
        let server = server::serve(Version::HTTP_11, vec![Some(response)]).await?;
        let client = TenancyClient::new(&server.endpoint, TOKEN)?;
        let result = client
            .resource_capabilities(&"installation".try_into()?)
            .await;
        if case == "disabled" {
            assert!(!result?.provisioning_enabled);
        } else {
            assert!(result.is_err(), "{case}");
        }
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].method, "GET");
        assert_eq!(records[0].path, "/control/v1/tenancy/resource-capabilities");
    }
    Ok(())
}

#[tokio::test]
async fn resource_put_refuses_invalid_input_and_preserves_uncertain_result_without_retry()
-> TestResult {
    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let mut invalid = input()?;
    let Definition::ProfiledRoute { attachments, .. } = &mut invalid.definition else {
        return Err("route".into());
    };
    attachments[0].candidate_index = 256;
    assert!(matches!(
        client.put_resource(&"tenant".try_into()?, &invalid).await,
        Err(Error::Request(_))
    ));
    assert!(server.records.lock().await.is_empty());
    for case in ["lost", "operation", "resource", "malformed"] {
        let mut response = applied();
        match case {
            "operation" => response["operation_id"] = json!("other"),
            "resource" => response["resource_id"] = json!("other"),
            "malformed" => response["status"] = json!("invented"),
            _ => {}
        }
        let server = server::serve(
            Version::HTTP_11,
            vec![if case == "lost" {
                None
            } else {
                Some(server::json(response)?)
            }],
        )
        .await?;
        let client = TenancyClient::new(&server.endpoint, TOKEN)?;
        assert!(
            matches!(
                client.put_resource(&"tenant".try_into()?, &input()?).await,
                Err(Error::TenancyUnknown { .. })
            ),
            "{case}"
        );
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}
