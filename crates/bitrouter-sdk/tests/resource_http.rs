//! Resource authority uses the bounded administration transport, never inference.

#[path = "transport_protocol/server.rs"]
mod server;

use axum::http::Version;
use bitrouter_ai::tenancy::{configuration::ResourceRef, resources::ResourcePut};
use bitrouter_sdk::{
    error::Error,
    gateway::HttpVersion,
    tenancy::{TenancyClient, TenancyOptions},
};
use serde_json::{Value, json};
use std::error::Error as StdError;

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TOKEN: &str = "fixture-resource-token-0123456789abcdef";

fn reference() -> TestResult<ResourceRef> {
    Ok(serde_json::from_value(
        json!({"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential","resource_id":"slot","revision_id":"revision"}),
    )?)
}
fn input() -> TestResult<ResourcePut> {
    Ok(serde_json::from_value(
        json!({"format":1,"operation_id":"operation","installation_id":"installation","key":{"owner_kind":"tenant","owner_id":"tenant","resource_kind":"credential","resource_id":"slot"},"expected_revision":null,"definition":{"kind":"credential","secret":"write-only-secret-canary"}}),
    )?)
}
fn applied() -> Value {
    json!({"operation_id":"operation","status":"applied","authority_revision":1,"resource_revision":"revision","resource_id":"slot","error":null})
}
fn status() -> TestResult<Value> {
    Ok(
        json!({"format":1,"installation_id":"installation","reference":reference()?,"authority_revision":1,"head_revision":"revision","revoked":false}),
    )
}

async fn roundtrip(version: Version, mode: HttpVersion) -> TestResult {
    let replies = vec![
        json!({"format":1,"installation_id":"installation","provisioning_enabled":true,"max_command_bytes":1048576,"tenant_routes_require_approved_origins":true}),
        applied(), status()?, applied(),
    ].into_iter().map(|body| server::json(body).map(Some)).collect::<TestResult<Vec<_>>>()?;
    let server = server::serve(version, replies).await?;
    let client = TenancyClient::with_options(
        &server.endpoint,
        TOKEN,
        TenancyOptions {
            http_version: mode,
            ..Default::default()
        },
    )?;
    let install = "installation".try_into()?;
    let tenant = "tenant".try_into()?;
    client.resource_capabilities(&install).await?;
    let result = client.put_resource(&tenant, &input()?).await?;
    assert_eq!(
        result.resource_revision.as_ref().map(|r| r.as_str()),
        Some("revision")
    );
    let read = client
        .resource_status(&install, &tenant, &reference()?)
        .await?;
    assert!(!read.revoked);
    client.revoke_resource(&tenant, &serde_json::from_value(json!({"format":1,"operation_id":"operation","installation_id":"installation","reference":reference()?,"expected_authority_revision":1}))?).await?;
    let records = server.records.lock().await;
    assert_eq!(records.len(), 4);
    for record in records.iter() {
        assert_eq!(record.version, version);
        assert_eq!(record.connection, records[0].connection);
        assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(record.headers["bitrouter-installation-id"], "installation");
    }
    assert_eq!(records[0].path, "/control/v1/tenancy/resource-capabilities");
    assert_eq!(records[0].method, "GET");
    assert_eq!(records[1].path, "/control/v1/tenants/tenant/resources");
    assert_eq!(records[1].method, "POST");
    let sent: Value = serde_json::from_slice(&records[1].body)?;
    assert_eq!(sent["expected_revision"], Value::Null);
    assert_eq!(sent["definition"]["secret"], "write-only-secret-canary");
    assert!(
        records[2]
            .path
            .starts_with("/control/v1/tenants/tenant/resources?")
    );
    for expected in [
        "owner_kind=tenant",
        "owner_id=tenant",
        "resource_kind=credential",
        "resource_id=slot",
        "revision_id=revision",
    ] {
        assert!(
            records[2]
                .path
                .split('?')
                .nth(1)
                .ok_or("query absent")?
                .split('&')
                .any(|pair| pair == expected)
        );
    }
    assert_eq!(
        records[3].path,
        "/control/v1/tenants/tenant/resources/revoke"
    );
    assert!(!format!("{:?}", input()?).contains("write-only-secret-canary"));
    Ok(())
}

#[tokio::test]
async fn resource_http1_transport_preserves_cas_and_scoped_identity() -> TestResult {
    roundtrip(Version::HTTP_11, HttpVersion::Http1Only).await
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn resource_h2c_transport_preserves_cas_and_scoped_identity() -> TestResult {
    roundtrip(Version::HTTP_2, HttpVersion::Http2PriorKnowledge).await
}

#[tokio::test]
async fn resource_reads_reject_wrong_installation_and_reference() -> TestResult {
    let mut wrong_install = status()?;
    wrong_install["installation_id"] = "other".into();
    let mut wrong_resource = status()?;
    wrong_resource["reference"]["resource_id"] = "other".into();
    let server = server::serve(
        Version::HTTP_11,
        vec![
            Some(server::json(wrong_install)?),
            Some(server::json(wrong_resource)?),
        ],
    )
    .await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    for _ in 0..2 {
        assert!(matches!(
            client
                .resource_status(
                    &"installation".try_into()?,
                    &"tenant".try_into()?,
                    &reference()?
                )
                .await,
            Err(Error::Response(_))
        ));
    }
    assert_eq!(server.records.lock().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn lost_resource_write_is_unknown_and_never_resubmitted() -> TestResult {
    let server =
        server::serve(Version::HTTP_11, vec![None, Some(server::json(applied())?)]).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let input = input()?;
    let error = client
        .put_resource(&"tenant".try_into()?, &input)
        .await
        .err()
        .ok_or("expected unknown")?;
    assert!(
        matches!(&error,Error::TenancyUnknown {operation_id,..} if operation_id == &input.operation_id)
    );
    assert!(!format!("{error:?}").contains("write-only-secret-canary"));
    assert_eq!(server.records.lock().await.len(), 1);
    let recovered = client
        .operation(&input.installation_id, &input.operation_id)
        .await?;
    assert_eq!(
        recovered.resource_id.as_ref().map(|id| id.as_str()),
        Some("slot")
    );
    assert_eq!(server.records.lock().await.len(), 2);
    Ok(())
}
