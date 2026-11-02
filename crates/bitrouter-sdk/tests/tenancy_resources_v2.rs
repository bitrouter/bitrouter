//! Versioned provisioning uses real HTTP sockets and never silently downgrades.
#[path = "transport_protocol/server.rs"]
mod server;

use axum::{body::Body, http::Version, response::Response};
use bitrouter_ai::tenancy::resources_v2::{DefinitionV2, ResourcePutV2};
use bitrouter_sdk::{
    error::Error,
    gateway::HttpVersion,
    tenancy::{TenancyClient, TenancyOptions},
};
use serde_json::{Value, json};
type TestResult<T = ()> = Result<T, Box<dyn std::error::Error + Send + Sync>>;
const TOKEN: &str = "fixture-tenancy-token-0123456789abcdef";

fn capabilities() -> Value {
    json!({"format":2,"installation_id":"installation","provisioning_enabled":true,
        "max_command_bytes":1048576,"tenant_routes_require_approved_origins":true,
        "admission_profile_versions":[1,2]})
}

fn input() -> TestResult<ResourcePutV2> {
    Ok(serde_json::from_value(
        json!({"format":2,"operation_id":"operation","installation_id":"installation",
        "key":{"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"member"},
        "expected_revision":null,"definition":{"kind":"admission_profile_v2","profile":{"schema_version":2,
            "rate":{"rate_group_id":"window","member_rpm":3},"health":null,"health_group_id":null,"fallback":"stop"}}}),
    )?)
}

fn applied() -> Value {
    json!({"operation_id":"operation","status":"applied","authority_revision":1,
        "resource_revision":"revision","resource_id":"member","error":null})
}

#[tokio::test]
async fn resource_v2_discovery_and_put_bind_installation_and_preserve_v1_routes() -> TestResult {
    for (transport, version) in [
        (HttpVersion::Http1Only, Version::HTTP_11),
        #[cfg(feature = "http2")]
        (HttpVersion::Http2PriorKnowledge, Version::HTTP_2),
    ] {
        let mut old_caps = capabilities();
        old_caps["format"] = json!(1);
        old_caps
            .as_object_mut()
            .ok_or("capabilities")?
            .remove("admission_profile_versions");
        let server = server::serve(
            version,
            vec![
                Some(server::json(capabilities())?),
                Some(server::json(applied())?),
                Some(server::json(old_caps)?),
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
            .resource_capabilities_v2(&request.installation_id)
            .await?;
        assert!(caps.provisioning_enabled);
        assert_eq!(caps.admission_profile_versions, [1, 2]);
        let result = client
            .put_resource_v2(&"tenant".try_into()?, &request)
            .await?;
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
            ("GET", "/control/v1/tenancy/resource-capabilities/v2"),
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
        "v1",
        "foreign",
        "versions",
        "fraction",
        "duplicate",
        "disabled",
    ] {
        let mut value = capabilities();
        match case {
            "v1" => {
                value["format"] = json!(1);
                value
                    .as_object_mut()
                    .ok_or("capabilities")?
                    .remove("admission_profile_versions");
            }
            "foreign" => value["installation_id"] = json!("foreign"),
            "versions" => value["admission_profile_versions"] = json!([1, 3]),
            "disabled" => value["provisioning_enabled"] = json!(false),
            _ => {}
        }
        let mut raw = value.to_string();
        if case == "fraction" {
            raw = raw.replace("\"format\":2", "\"format\":2.00000000000000001");
        }
        if case == "duplicate" {
            raw = raw.replace("\"format\":2", "\"format\":2,\"format\":2");
        }
        let response = Response::builder()
            .status(if case == "not_found" { 404 } else { 200 })
            .header("content-type", "application/json")
            .body(Body::from(raw))?;
        let server = server::serve(Version::HTTP_11, vec![Some(response)]).await?;
        let client = TenancyClient::new(&server.endpoint, TOKEN)?;
        let result = client
            .resource_capabilities_v2(&"installation".try_into()?)
            .await;
        if case == "disabled" {
            assert!(!result?.provisioning_enabled);
        } else {
            assert!(result.is_err(), "{case}");
        }
        let records = server.records.lock().await;
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].method, "GET");
        assert_eq!(
            records[0].path,
            "/control/v1/tenancy/resource-capabilities/v2"
        );
    }
    Ok(())
}

#[tokio::test]
async fn v2_put_refuses_invalid_input_and_preserves_uncertain_result_without_retry() -> TestResult {
    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let mut invalid = input()?;
    let DefinitionV2::AdmissionProfileV2 { profile } = &mut invalid.definition;
    profile.health_group_id = Some("group-without-health".try_into()?);
    assert!(matches!(
        client
            .put_resource_v2(&"tenant".try_into()?, &invalid)
            .await,
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
                client
                    .put_resource_v2(&"tenant".try_into()?, &input()?)
                    .await,
                Err(Error::TenancyUnknown { .. })
            ),
            "{case}"
        );
        assert_eq!(server.records.lock().await.len(), 1);
    }
    Ok(())
}
