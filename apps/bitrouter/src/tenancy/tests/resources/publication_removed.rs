//! Removed platform writers stay unavailable even without a configured catalog.

use super::*;
use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use bitrouter_ai::control::ControlScope;
use serde_json::json;
use tower::ServiceExt;

#[tokio::test]
async fn removed_platform_formats_and_capabilities_do_not_write_without_catalog() -> Result<()> {
    let fixture = keyed_fixture().await?;
    ensure!(fixture.service.resources.configured().is_none());
    let token = format!("test-operator-{}", uuid::Uuid::new_v4());
    let operators = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                id: "test-operator".into(),
                token_env: "TEST_REMOVED_PLATFORM_TOKEN".into(),
                scopes: vec![ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some(token.clone()),
    )?;
    let app = crate::tenancy::http::router(fixture.service.clone(), operators);
    let before = fixture.service.current().map_err(failure)?;
    for version in [2, 3] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!(
                        "/control/v1/tenancy/resource-capabilities/v{version}"
                    ))
                    .header("authorization", format!("Bearer {token}"))
                    .header("Bitrouter-Installation-Id", fixture.installation.as_str())
                    .body(Body::empty())?,
            )
            .await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
    }
    let slot = json!({"owner_kind":"tenant","owner_id":fixture.tenant,"resource_kind":"credential","resource_id":"slot","revision_id":"old-slot"});
    let profile_ref = json!({"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"profile","revision_id":"old-profile"});
    for (format, definition) in [
        (
            1,
            json!({"kind":"admission_profile","profile":{"schema_version":1,"rate":null,"health":null,"fallback":"stop"}}),
        ),
        (
            2,
            json!({"kind":"admission_profile_v2","profile":{"schema_version":2,"rate":null,"health":null,"health_group_id":null,"fallback":"stop"}}),
        ),
        (
            1,
            json!({"kind":"profiled_route","routing":{},"selector":"provider/model","policy":null,
            "credentials":[{"provider":"provider","account":"default","credential":slot}],
            "attachments":[{"inbound_protocol":"chat_completions","candidate_index":0,"credential":slot,"profile":profile_ref}]}),
        ),
        (
            3,
            json!({"kind":"native_profiled_route_v1","routing":{},"selector":"provider/model","policy":null,
            "credentials":[{"provider":"provider","account":"default","credential":slot}],
            "attachments":[{"inbound_protocol":"generate_content","candidate_index":0,"credential":slot,"profile":profile_ref}]}),
        ),
    ] {
        let command = json!({"format":format,"operation_id":"removed-platform-write","installation_id":fixture.installation,
            "key":{"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy","resource_id":"profile"},
            "expected_revision":null,"definition":definition});
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(format!("/control/v1/tenants/{}/resources", fixture.tenant))
                    .header("authorization", format!("Bearer {token}"))
                    .header("Bitrouter-Installation-Id", fixture.installation.as_str())
                    .header("content-type", "application/json")
                    .body(Body::from(serde_json::to_vec(&command)?))?,
            )
            .await?;
        ensure!(response.status() == StatusCode::BAD_REQUEST);
    }
    let after = fixture.service.current().map_err(failure)?;
    ensure!(before.revision == after.revision && before.operation_ids == after.operation_ids);
    let row = fixture
        .db
        .query_one(Statement::from_string(
            DatabaseBackend::Sqlite,
            "SELECT count(*) AS n FROM tenancy_resource_revisions",
        ))
        .await?
        .context("resource count")?;
    ensure!(row.try_get::<i64>("", "n")? == 0);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn installation_pool_mutations_are_denied_while_tenant_credentials_still_work() -> Result<()>
{
    let fixture = keyed_fixture().await?;
    let before = fixture.service.current().map_err(failure)?;
    let mut input = credential(&fixture, "blocked-pool-create")?;
    input.key.owner_kind = OwnerKind::OperatorPool;
    input.key.owner_id = "pool".try_into()?;
    let denied = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::Resource(fixture.tenant.clone(), input),
        )
        .await;
    ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden));
    let denied = fixture
        .service
        .execute(
            fixture.operator.clone(),
            Command::RevokeResource(
                fixture.tenant.clone(),
                ResourceRevoke {
                    format: 1,
                    operation_id: "blocked-pool-revoke".try_into()?,
                    installation_id: fixture.installation.clone(),
                    expected_authority_revision: before.revision,
                    reference: ResourceRef {
                        owner_kind: OwnerKind::OperatorPool,
                        owner_id: "pool".try_into()?,
                        resource_kind: ResourceKind::Credential,
                        resource_id: "slot".try_into()?,
                        revision_id: "old-slot".try_into()?,
                    },
                },
            ),
        )
        .await;
    ensure!(matches!(denied, Err(error) if error.0.code == ErrorCode::Forbidden));
    let after = fixture.service.current().map_err(failure)?;
    ensure!(before.revision == after.revision && before.operation_ids == after.operation_ids);
    let mut input = credential(&fixture, "retained-user-create")?;
    input.definition = Definition::Credential {
        secret: uuid::Uuid::new_v4().to_string(),
    };
    let applied = put(&fixture, fixture.operator.clone(), input).await?;
    ensure!(applied.status == bitrouter_ai::tenancy::operation::OperationStatus::Applied);
    fixture.service.shutdown().await?;
    Ok(())
}
