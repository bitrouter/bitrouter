//! Tenancy transport depends only on public contracts and real HTTP sockets.

#[path = "transport_protocol/server.rs"]
mod server;

use std::{error::Error as StdError, time::Duration};

use axum::{body::Body, http::Version, response::Response};
use bitrouter_ai::tenancy::{
    authority::TenantPut,
    identity::{InstallationId, TenantId},
};
use bitrouter_sdk::{
    error::Error,
    gateway::HttpVersion,
    tenancy::{TenancyClient, TenancyOptions},
};
use serde_json::{Value, json};

type TestResult<T = ()> = Result<T, Box<dyn StdError + Send + Sync>>;
const TOKEN: &str = "fixture-tenancy-token-0123456789abcdef";

fn capabilities(boot: &str) -> Value {
    json!({"contract_version":1,"installation_id":"installation","instance_id":boot,"capability_revision":"capability","revision_bound_dispatch":true,"strict_receipts":true,"dispatch_enabled":true})
}
fn applied() -> Value {
    json!({"operation_id":"operation","status":"applied","authority_revision":1,"resource_revision":"revision","resource_id":"tenant","error":null})
}
fn applied_to(resource: &str) -> Value {
    let mut result = applied();
    result["resource_id"] = resource.into();
    result
}
fn configuration() -> Value {
    json!({"routes":[],"policy":null,"generation_defaults":{"max_output_tokens":100,"temperature":1.0},"learning":{"mode":"off","max_evaluations":0},"limits":{"active_executions":16,"queued_work":32,"learning_jobs":2}})
}
fn revision() -> Value {
    json!({"tenant_id":"tenant","revision_id":"revision","schema_version":1,"compiler_version":"1","content_digest":"a".repeat(64),"document":configuration()})
}
fn issued() -> Value {
    json!({"operation_id":"operation","credential_id":"credential","credential_revision":"revision","principal_id":"principal","tenant_id":"tenant","authority_revision":1,"secret":"secret-only-once-canary","expires_at":"2030-01-01T00:00:00Z"})
}
fn empty_usage() -> Value {
    json!({"input_tokens":null,"output_tokens":null,"cache_read_tokens":null,"cache_write_tokens":null,"reasoning_tokens":null,"provenance":"unknown","complete":false})
}
fn receipt() -> Value {
    json!({"installation_id":"installation","tenant_id":"tenant","principal_id":"principal","execution_id":"execution","correlation_id":"correlation","execution_revision":"revision","state":"unknown","outcome":"unknown","attempts":[],"usage":empty_usage(),"recorded_at":"2026-10-25T00:00:00Z"})
}
fn tenant_input() -> TestResult<TenantPut> {
    Ok(serde_json::from_value(
        json!({"operation_id":"operation","installation_id":"installation","expected_revision":null,"state":"active"}),
    )?)
}
fn rejection(code: &str, operation: &str) -> Value {
    json!({"code":code,"message":"redacted service error","dispatch_state":"not_started","operation_id":operation,"execution_id":null})
}

#[tokio::test]
async fn all_sixteen_operations_use_the_frozen_routes_and_installation_binding() -> TestResult {
    let replies = vec![
        capabilities("boot"),
        applied(),
        applied_to("principal"),
        applied_to("binding"),
        issued(),
        json!({"tenant_id":"tenant","credential_id":"credential","credential_revision":"revision","authority_revision":1,"state":"active"}),
        applied_to("credential"),
        issued(),
        applied_to("binding"),
        revision(),
        applied(),
        json!({"tenant_id":"tenant","revision_id":"revision","authority_revision":1}),
        revision(),
        applied(),
        receipt(),
        json!({"installation_id":"installation","correlation_id":"correlation","execution_ids":["execution-a","execution-b"],"multiple_executions":true,"next_after":"execution-b"}),
    ];
    let replies = replies
        .into_iter()
        .map(|value| server::json(value).map(Some))
        .collect::<TestResult<Vec<_>>>()?;
    let server = server::serve(Version::HTTP_11, replies).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let install: InstallationId = "installation".try_into()?;
    let tenant: TenantId = "tenant".try_into()?;
    let principal = "principal".try_into()?;
    let binding = "binding".try_into()?;
    let credential = "credential".try_into()?;
    let revision_id = "revision".try_into()?;
    let operation = "operation".try_into()?;
    let mutation = serde_json::from_value(
        json!({"operation_id":"operation","installation_id":"installation","expected_revision":"revision"}),
    )?;
    assert_eq!(client.capabilities().await?.instance_id.as_str(), "boot");
    client.put_tenant(&tenant, &tenant_input()?).await?;
    client.put_principal(&principal, &serde_json::from_value(json!({"operation_id":"operation","installation_id":"installation","expected_revision":null,"issuer":"test","subject":"subject"}))?).await?;
    client.put_binding(&tenant, &binding, &serde_json::from_value(json!({"operation_id":"operation","installation_id":"installation","expected_revision":null,"principal_id":"principal","role":"tenant_admin","scope":{"kind":"tenant"},"expires_at":"2030-01-01T00:00:00Z","expected_management_owner":null}))?).await?;
    let response = client.issue_credential(&tenant, &serde_json::from_value(json!({"operation_id":"operation","installation_id":"installation","expected_authority_revision":1,"principal_id":"principal","permissions":["resource.read"],"scope":{"kind":"tenant"},"expires_at":"2030-01-01T00:00:00Z","rotate_credential_id":null,"binding_id":"binding"}))?).await?;
    assert_eq!(response.secret.as_deref(), Some("secret-only-once-canary"));
    let issued_client = TenancyClient::new(
        &server.endpoint,
        response.secret.as_deref().ok_or("missing initial secret")?,
    )?;
    assert!(!format!("{issued_client:?}").contains("secret-only-once-canary"));
    assert!(!format!("{response:?}").contains("secret-only-once-canary"));
    client.credential(&install, &tenant, &credential).await?;
    client
        .revoke_credential(&tenant, &credential, &mutation)
        .await?;
    client.issue_audit_reader(&tenant, &serde_json::from_value(json!({"operation_id":"operation","installation_id":"installation","expected_authority_revision":1,"principal_id":"principal","expires_at":"2030-01-01T00:00:00Z"}))?).await?;
    client
        .detach_binding_management(&tenant, &binding, &mutation)
        .await?;
    client.put_configuration(&tenant, &serde_json::from_value(json!({"operation_id":"operation","installation_id":"installation","expected_authority_revision":1,"document":configuration()}))?).await?;
    client.activate(&tenant, &serde_json::from_value(json!({"operation_id":"operation","installation_id":"installation","expected_authority_revision":1,"expected_active_revision":null,"revision_id":"revision"}))?).await?;
    client.active_revision(&install, &tenant).await?;
    client
        .configuration(&install, &tenant, &revision_id)
        .await?;
    client.operation(&install, &operation).await?;
    client
        .execution(&install, &tenant, &"execution".try_into()?)
        .await?;
    let page = client
        .executions(
            &install,
            &tenant,
            &"correlation".try_into()?,
            Some(&"execution-0".try_into()?),
            2,
        )
        .await?;
    assert!(page.multiple_executions);
    let expected = [
        ("GET", "tenancy/capabilities"),
        ("PUT", "tenants/tenant"),
        ("PUT", "principals/principal"),
        ("PUT", "tenants/tenant/bindings/binding"),
        ("POST", "tenants/tenant/credentials"),
        ("GET", "tenants/tenant/credentials/credential"),
        ("DELETE", "tenants/tenant/credentials/credential"),
        ("POST", "tenants/tenant/audit-readers"),
        ("DELETE", "tenants/tenant/bindings/binding/management"),
        ("POST", "tenants/tenant/configurations"),
        ("POST", "tenants/tenant/activation"),
        ("GET", "tenants/tenant/activation"),
        ("GET", "tenants/tenant/configurations/revision"),
        ("GET", "tenancy/operations/operation"),
        ("GET", "tenants/tenant/executions/execution"),
        (
            "GET",
            "tenants/tenant/executions?correlation_id=correlation&limit=2&after=execution-0",
        ),
    ];
    let records = server.records.lock().await;
    assert_eq!(records.len(), expected.len());
    for (i, (record, (method, path))) in records.iter().zip(expected).enumerate() {
        assert_eq!(record.method, method);
        assert_eq!(record.path, format!("/control/v1/{path}"));
        assert_eq!(record.version, Version::HTTP_11);
        assert_eq!(record.headers["authorization"], format!("Bearer {TOKEN}"));
        assert_eq!(record.connection, records[0].connection);
        if i == 0 {
            assert!(record.headers.get("Bitrouter-Installation-Id").is_none());
        } else {
            assert_eq!(record.headers["Bitrouter-Installation-Id"], "installation");
        }
        if method != "GET" {
            let body: Value = serde_json::from_slice(&record.body)?;
            assert_eq!(body["installation_id"], "installation");
            assert_eq!(body["operation_id"], "operation");
        } else {
            assert!(record.body.is_empty());
        }
    }
    Ok(())
}

#[tokio::test]
async fn invalid_local_input_never_reaches_the_listener() -> TestResult {
    let server = server::serve(Version::HTTP_11, vec![]).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let tenant = "tenant".try_into()?;
    let input = serde_json::from_value(
        json!({"operation_id":"operation","installation_id":"installation","expected_authority_revision":1,"principal_id":"principal","permissions":[],"scope":{"kind":"tenant"},"expires_at":"2030-01-01T00:00:00Z","rotate_credential_id":null,"binding_id":"binding"}),
    )?;
    assert!(matches!(
        client.issue_credential(&tenant, &input).await,
        Err(Error::Request(_))
    ));
    assert!(matches!(
        client
            .executions(
                &"installation".try_into()?,
                &tenant,
                &"correlation".try_into()?,
                None,
                101
            )
            .await,
        Err(Error::Request(_))
    ));
    assert!(server.records.lock().await.is_empty());
    Ok(())
}

#[tokio::test]
async fn lost_mutation_response_requires_explicit_recovery_without_retry() -> TestResult {
    let server =
        server::serve(Version::HTTP_11, vec![None, Some(server::json(applied())?)]).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let input = tenant_input()?;
    let error = client
        .put_tenant(&"tenant".try_into()?, &input)
        .await
        .err()
        .ok_or("expected loss")?;
    assert!(
        matches!(&error, Error::TenancyUnknown { operation_id, .. } if operation_id == &input.operation_id)
    );
    assert!(!format!("{error:?}").contains(TOKEN));
    assert_eq!(server.records.lock().await.len(), 1);
    let result = client
        .operation(&input.installation_id, &input.operation_id)
        .await?;
    assert_eq!(result.operation_id, input.operation_id);
    assert_eq!(server.records.lock().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn redirects_do_not_forward_credentials_or_turn_into_success() -> TestResult {
    let destination = server::serve(Version::HTTP_11, vec![Some(server::json(applied())?)]).await?;
    let reply = Response::builder()
        .status(307)
        .header("location", &destination.endpoint)
        .header("content-type", "application/json")
        .body(Body::from("{}"))?;
    let source = server::serve(Version::HTTP_11, vec![Some(reply)]).await?;
    let client = TenancyClient::new(&source.endpoint, TOKEN)?;
    assert!(matches!(
        client
            .put_tenant(&"tenant".try_into()?, &tenant_input()?)
            .await,
        Err(Error::TenancyUnknown { .. })
    ));
    assert!(destination.records.lock().await.is_empty());
    assert_eq!(source.records.lock().await.len(), 1);
    Ok(())
}

#[tokio::test]
async fn definite_rejections_need_matching_status_identity_and_dispatch_evidence() -> TestResult {
    let mut wrong_id = rejection("revision_conflict", "other-operation");
    wrong_id["message"] = "sensitive-error-canary".into();
    let values = [
        (409, rejection("revision_conflict", "operation")),
        (409, wrong_id),
        (503, rejection("storage_unavailable", "operation")),
    ];
    let replies = values
        .into_iter()
        .map(|(status, body)| {
            Ok(Some(
                Response::builder()
                    .status(status)
                    .header("content-type", "application/json")
                    .body(Body::from(body.to_string()))?,
            ))
        })
        .collect::<TestResult<Vec<_>>>()?;
    let server = server::serve(Version::HTTP_11, replies).await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let tenant = "tenant".try_into()?;
    let input = tenant_input()?;
    assert!(matches!(
        client.put_tenant(&tenant, &input).await,
        Err(Error::Tenancy(_))
    ));
    let error = client
        .put_tenant(&tenant, &input)
        .await
        .err()
        .ok_or("expected mismatch")?;
    assert!(matches!(error, Error::TenancyUnknown { .. }));
    assert!(!format!("{error:?}").contains("sensitive-error-canary"));
    assert!(matches!(
        client.put_tenant(&tenant, &input).await,
        Err(Error::TenancyUnknown { .. })
    ));
    Ok(())
}

#[tokio::test]
async fn malformed_duplicate_oversized_and_encoded_responses_are_rejected() -> TestResult {
    let duplicate = r#"{"contract_version":1,"contract_version":1,"installation_id":"installation","instance_id":"boot","capability_revision":"cap","revision_bound_dispatch":true,"strict_receipts":true,"dispatch_enabled":true}"#;
    let replies = vec![
        Some(
            Response::builder()
                .header("content-type", "application/json")
                .body(Body::from(duplicate))?,
        ),
        Some(server::json(json!({"contract_version":99}))?),
        Some(
            Response::builder()
                .header("content-type", "application/json")
                .header("content-encoding", "gzip")
                .body(Body::from("compressed-canary"))?,
        ),
        Some(server::json(json!({"padding":"x".repeat(1024)}))?),
    ];
    let server = server::serve(Version::HTTP_11, replies).await?;
    let client = TenancyClient::with_options(
        &server.endpoint,
        TOKEN,
        TenancyOptions {
            max_response_bytes: 512,
            ..Default::default()
        },
    )?;
    for _ in 0..4 {
        assert!(matches!(
            client.capabilities().await,
            Err(Error::Tenancy(_))
        ));
    }
    assert_eq!(server.records.lock().await.len(), 4);
    Ok(())
}

#[tokio::test]
async fn returned_ownership_and_page_bounds_are_checked() -> TestResult {
    let mut wrong_receipt = receipt();
    wrong_receipt["tenant_id"] = "foreign-tenant".into();
    let wrong_page = json!({"installation_id":"installation","correlation_id":"correlation","execution_ids":["execution-a","execution-b"],"multiple_executions":true,"next_after":null});
    let mut wrong_operation = applied();
    wrong_operation["operation_id"] = "foreign-operation".into();
    let server = server::serve(
        Version::HTTP_11,
        vec![
            Some(server::json(wrong_receipt)?),
            Some(server::json(wrong_page)?),
            Some(server::json(wrong_operation)?),
        ],
    )
    .await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let install = "installation".try_into()?;
    let tenant = "tenant".try_into()?;
    assert!(matches!(
        client
            .execution(&install, &tenant, &"execution".try_into()?)
            .await,
        Err(Error::Response(_))
    ));
    assert!(matches!(
        client
            .executions(&install, &tenant, &"correlation".try_into()?, None, 1)
            .await,
        Err(Error::Response(_))
    ));
    assert!(matches!(
        client.put_tenant(&tenant, &tenant_input()?).await,
        Err(Error::TenancyUnknown { .. })
    ));
    Ok(())
}

#[test]
fn control_endpoint_and_transport_options_stay_explicit_and_bounded() {
    for endpoint in [
        "http://remote.invalid",
        "https://user:password@example.com",
        "https://example.com?x=1",
        "https://example.com/path",
        "file:///tmp/test",
    ] {
        assert!(TenancyClient::new(endpoint, TOKEN).is_err());
    }
    for token in [
        "",
        "token containing spaces 0123456789abcdef",
        "token\nheader-injection-0123456789abcdef",
    ] {
        assert!(TenancyClient::new("https://example.com", token).is_err());
    }
    assert!(
        TenancyClient::with_options(
            "https://example.com",
            TOKEN,
            TenancyOptions {
                request_timeout: Duration::ZERO,
                ..Default::default()
            }
        )
        .is_err()
    );
    assert!(
        TenancyClient::with_options(
            "https://example.com",
            TOKEN,
            TenancyOptions {
                max_response_bytes: 17 * 1024 * 1024,
                ..Default::default()
            }
        )
        .is_err()
    );
}

#[tokio::test]
async fn applied_revocation_and_activation_must_match_the_requested_resource() -> TestResult {
    let mut wrong_revision = applied();
    wrong_revision["resource_revision"] = "unrequested-revision".into();
    let server = server::serve(
        Version::HTTP_11,
        vec![
            Some(server::json(applied_to("other-credential"))?),
            Some(server::json(wrong_revision)?),
        ],
    )
    .await?;
    let client = TenancyClient::new(&server.endpoint, TOKEN)?;
    let tenant = "tenant".try_into()?;
    let input = serde_json::from_value(
        json!({"operation_id":"operation","installation_id":"installation","expected_revision":"revision"}),
    )?;
    assert!(matches!(
        client
            .revoke_credential(&tenant, &"credential".try_into()?, &input)
            .await,
        Err(Error::TenancyUnknown { .. })
    ));
    let activation = serde_json::from_value(
        json!({"operation_id":"operation","installation_id":"installation","expected_authority_revision":1,"expected_active_revision":null,"revision_id":"revision"}),
    )?;
    assert!(matches!(
        client.activate(&tenant, &activation).await,
        Err(Error::TenancyUnknown { .. })
    ));
    assert_eq!(server.records.lock().await.len(), 2);
    Ok(())
}

#[tokio::test]
async fn capabilities_are_fresh_and_http1_does_not_depend_on_unified_features() -> TestResult {
    for version in [HttpVersion::Auto, HttpVersion::Http1Only] {
        let server = server::serve(
            Version::HTTP_11,
            vec![
                Some(server::json(capabilities("boot-a"))?),
                Some(server::json(capabilities("boot-b"))?),
            ],
        )
        .await?;
        let client = TenancyClient::with_options(
            &server.endpoint,
            TOKEN,
            TenancyOptions {
                http_version: version,
                ..Default::default()
            },
        )?;
        assert_eq!(client.capabilities().await?.instance_id.as_str(), "boot-a");
        assert_eq!(client.capabilities().await?.instance_id.as_str(), "boot-b");
        let records = server.records.lock().await;
        assert_eq!(records.len(), 2);
        assert!(records.iter().all(|r| r.version == Version::HTTP_11));
    }
    Ok(())
}

#[cfg(feature = "http2")]
#[tokio::test]
async fn h2c_uses_the_same_control_routes_and_connection_without_fallback() -> TestResult {
    let server = server::serve(
        Version::HTTP_2,
        vec![
            Some(server::json(capabilities("boot"))?),
            Some(server::json(applied())?),
        ],
    )
    .await?;
    let client = TenancyClient::with_options(
        &server.endpoint,
        TOKEN,
        TenancyOptions {
            http_version: HttpVersion::Http2PriorKnowledge,
            ..Default::default()
        },
    )?;
    client.capabilities().await?;
    client
        .put_tenant(&"tenant".try_into()?, &tenant_input()?)
        .await?;
    let records = server.records.lock().await;
    assert_eq!(records.len(), 2);
    assert!(records.iter().all(|r| r.version == Version::HTTP_2));
    assert_eq!(records[0].connection, records[1].connection);
    assert_eq!(records[1].path, "/control/v1/tenants/tenant");
    Ok(())
}
