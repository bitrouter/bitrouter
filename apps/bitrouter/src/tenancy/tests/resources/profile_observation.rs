//! Standalone retained profiles use the real writer, authority and sampler.
use super::*;
use axum::{
    body::Body,
    http::{Request as HttpRequest, StatusCode},
};
use bitrouter_ai::{
    control::ControlScope,
    tenancy::{
        admission::{AvailabilityObservation, HealthObservation},
        identity::RevisionId,
        profile_observation::{
            AdmissionProfileObservationCapabilitiesV1 as Capabilities,
            AdmissionProfileObservationRequestV1 as Request,
            AdmissionProfileObservationSnapshotV1 as Snapshot,
        },
        resources_v2::{DefinitionV2, ResourcePutV2},
    },
};
use serde_json::{Value, json};
use std::time::Duration;
use tokio::time::Instant;
use tower::ServiceExt;

mod runtime;
mod transport;
const TOKEN: &str = "profile-observation-test-operator-token-at-least-32-bytes";
const PATH: &str = "/control/v1/tenancy/admission-profiles/observations/v1";
const CAPS: &str = "/control/v1/tenancy/admission-profiles/observations/capabilities";

fn rules(version: u32, rpm: Option<u32>, health: bool) -> Value {
    let mut value = json!({"schema_version":version,"rate":rpm.map(|rpm|json!({"rate_group_id":"window","member_rpm":rpm})),"health":if health {json!({"rules":"availability_v1","failure_threshold":1,"transient_cooldown_seconds":10,"persistent_cooldown_seconds":30,"probe_lease_seconds":5})} else {Value::Null},"fallback":"stop"});
    if version == 2 {
        value["health_group_id"] = if health {
            json!("circuit")
        } else {
            Value::Null
        };
    }
    value
}
async fn provision(
    fixture: &Fixture,
    name: &str,
    operation: &str,
    expected: Option<RevisionId>,
    rules: Value,
) -> Result<ResourceRef> {
    provision_in_pool(fixture, "pool", name, operation, expected, rules).await
}
async fn provision_in_pool(
    fixture: &Fixture,
    pool: &str,
    name: &str,
    operation: &str,
    expected: Option<RevisionId>,
    rules: Value,
) -> Result<ResourceRef> {
    let key = ResourceKey {
        owner_kind: OwnerKind::OperatorPool,
        owner_id: pool.try_into()?,
        resource_kind: ResourceKind::Policy,
        resource_id: name.try_into()?,
    };
    let command = if rules["schema_version"] == 2 {
        Command::ResourceV2(
            fixture.tenant.clone(),
            ResourcePutV2 {
                format: 2,
                operation_id: operation.try_into()?,
                installation_id: fixture.installation.clone(),
                key: key.clone(),
                expected_revision: expected,
                definition: DefinitionV2::AdmissionProfileV2 {
                    profile: serde_json::from_value(rules)?,
                },
            },
        )
    } else {
        Command::Resource(
            fixture.tenant.clone(),
            ResourcePut {
                format: 1,
                operation_id: operation.try_into()?,
                installation_id: fixture.installation.clone(),
                key: key.clone(),
                expected_revision: expected,
                definition: Definition::AdmissionProfile {
                    profile: serde_json::from_value(rules)?,
                },
            },
        )
    };
    let result = result(
        fixture
            .service
            .execute(fixture.operator.clone(), command)
            .await
            .map_err(failure)?,
    )?;
    Ok(ResourceRef {
        owner_kind: key.owner_kind,
        owner_id: key.owner_id,
        resource_kind: key.resource_kind,
        resource_id: key.resource_id,
        revision_id: result.resource_revision.context("revision")?,
    })
}
fn request(fixture: &Fixture, profiles: Vec<ResourceRef>) -> Result<Request> {
    Ok(Request {
        schema_version: 1,
        expected_authority_revision: fixture.service.current().map_err(failure)?.revision,
        profiles,
    })
}
fn read(fixture: &Fixture, request: &Request) -> Result<Snapshot> {
    let read = fixture
        .service
        .profile_observation_read(
            &fixture.operator,
            Some(&fixture.installation),
            Instant::now() + Duration::from_secs(5),
        )
        .map_err(failure)?;
    let snapshot = read.sample(request).map_err(failure)?;
    Ok(serde_json::from_slice(
        &read.finish(&snapshot).map_err(failure)?,
    )?)
}
fn operational(fixture: &Fixture) -> Result<String> {
    fixture
        .service
        .operational
        .test_state()
        .map_err(|e| anyhow::anyhow!("runtime {e:?}"))
}
async fn durable(fixture: &Fixture) -> Result<Vec<(String, Vec<String>)>> {
    let tables=fixture.db.query_all(Statement::from_string(DatabaseBackend::Sqlite,"SELECT name FROM sqlite_schema WHERE type='table' AND name LIKE 'tenancy_%' ORDER BY name".to_owned())).await?;
    let mut snapshot = Vec::new();
    for row in tables {
        let table: String = row.try_get("", "name")?;
        ensure!(
            table
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'_')
        );
        let columns = fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("PRAGMA table_info(\"{table}\")"),
            ))
            .await?;
        let mut fields = Vec::new();
        for column in columns {
            let name: String = column.try_get("", "name")?;
            ensure!(name.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'));
            fields.push(format!("json_array(typeof(\"{name}\"),hex(\"{name}\"))"));
        }
        let rows = fixture
            .db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!(
                    "SELECT json_array({}) AS encoded FROM \"{table}\" ORDER BY encoded",
                    fields.join(",")
                ),
            ))
            .await?;
        snapshot.push((
            table,
            rows.into_iter()
                .map(|row| row.try_get::<String>("", "encoded"))
                .collect::<std::result::Result<Vec<_>, _>>()?,
        ));
    }
    Ok(snapshot)
}
fn operators() -> Result<crate::remote_control::auth::ControlAuth> {
    crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                id: "test-operator".into(),
                token_env: "TEST_PROFILE_OBSERVATION_OPERATOR".into(),
                scopes: vec![ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some(TOKEN.into()),
    )
}
fn router(fixture: &Fixture) -> Result<axum::Router> {
    Ok(crate::tenancy::http::router(
        fixture.service.clone(),
        operators()?,
    ))
}
fn http_request(fixture: &Fixture, input: &Request) -> Result<HttpRequest<Body>> {
    Ok(HttpRequest::builder()
        .method("POST")
        .uri(PATH)
        .header("authorization", format!("Bearer {TOKEN}"))
        .header("bitrouter-installation-id", fixture.installation.as_str())
        .header("content-type", "application/json")
        .body(Body::from(serde_json::to_vec(input)?))?)
}
async fn revoke(fixture: &Fixture, reference: ResourceRef, name: &str) -> Result<()> {
    result(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::RevokeResource(
                    fixture.tenant.clone(),
                    ResourceRevoke {
                        format: 1,
                        operation_id: name.try_into()?,
                        installation_id: fixture.installation.clone(),
                        reference,
                        expected_authority_revision: fixture
                            .service
                            .current()
                            .map_err(failure)?
                            .revision,
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(())
}

#[tokio::test]
async fn standalone_versions_observe_without_routes_configurations_or_credentials() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let local = provision(
        &fixture,
        "local",
        "create-local",
        None,
        rules(1, Some(0), true),
    )
    .await?;
    let shared = provision(
        &fixture,
        "shared",
        "create-shared",
        None,
        rules(2, None, false),
    )
    .await?;
    for table in ["tenancy_configurations", "tenancy_credentials"] {
        let row = fixture
            .db
            .query_one(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT COUNT(*) AS n FROM {table}"),
            ))
            .await?
            .context("count")?;
        ensure!(row.try_get::<i64>("", "n")? == 0);
    }
    let state = fixture.service.current().map_err(failure)?;
    ensure!(state.active_configurations.is_empty());
    drop(state);
    let before = durable(&fixture).await?;
    let runtime = operational(&fixture)?;
    let input = request(&fixture, vec![shared.clone(), local.clone()])?;
    let observed = read(&fixture, &input)?;
    ensure!(observed.profiles[0].profile == shared && observed.profiles[1].profile == local);
    ensure!(
        observed.profiles[0].rate.is_none() && observed.profiles[0].observed_shared_count.is_none()
    );
    ensure!(
        observed.profiles[0].health == HealthObservation::Disabled
            && observed.profiles[0].availability == AvailabilityObservation::Eligible
    );
    ensure!(
        observed.profiles[1].health == HealthObservation::Healthy
            && observed.profiles[1].availability == AvailabilityObservation::RateLimited
    );
    ensure!(
        observed.profiles[1]
            .rate
            .as_ref()
            .context("rate")?
            .shared_count
            == 0
    );
    ensure!(runtime == operational(&fixture)? && before == durable(&fixture).await?);
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn current_head_rotation_retains_pins_and_both_revocation_boundaries() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let mut revoked_pins = Vec::new();
    for version in [1, 2] {
        let name = format!("profile-{version}");
        let pin = provision(
            &fixture,
            &name,
            &format!("create-{version}"),
            None,
            rules(version, Some(1), false),
        )
        .await?;
        let head = provision(
            &fixture,
            &name,
            &format!("rotate-{version}"),
            Some(pin.revision_id.clone()),
            rules(version, Some(7), false),
        )
        .await?;
        let input = request(&fixture, vec![pin.clone(), head.clone()])?;
        let observed = read(&fixture, &input)?;
        ensure!(observed.profiles[0].profile == pin);
        for entry in &observed.profiles {
            ensure!(entry.current_profile_revision.as_ref() == Some(&head.revision_id));
            ensure!(entry.rate.as_ref().context("rate")?.member_rpm == 7);
        }
        revoke(&fixture, pin.clone(), &format!("revoke-pin-{version}")).await?;
        let observed = read(&fixture, &request(&fixture, input.profiles.clone())?)?;
        ensure!(observed.profiles[0].availability == AvailabilityObservation::AuthorityDenied);
        ensure!(observed.profiles[1].availability == AvailabilityObservation::Eligible);
        revoked_pins.extend(input.profiles.clone());
        revoke(&fixture, head, &format!("revoke-head-{version}")).await?;
        let observed = read(&fixture, &request(&fixture, input.profiles.clone())?)?;
        ensure!(
            observed
                .profiles
                .iter()
                .all(|entry| entry.current_profile_revision.is_none()
                    && entry.rate.is_none()
                    && entry.observed_shared_count.is_none()
                    && entry.health == HealthObservation::Unavailable
                    && entry.availability == AvailabilityObservation::AuthorityDenied)
        );
    }
    restart(&mut fixture).await?;
    ensure!(
        read(&fixture, &request(&fixture, revoked_pins)?)?
            .profiles
            .iter()
            .all(|entry| entry.availability == AvailabilityObservation::AuthorityDenied)
    );
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn missing_nonprofile_and_full_ref_transplants_never_substitute_authority() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "profile", "create", None, rules(2, Some(5), true)).await?;
    let mut cases = vec![pin.clone()];
    for field in 0..3 {
        let mut missing = pin.clone();
        match field {
            0 => missing.owner_id = "other-pool".try_into()?,
            1 => missing.resource_id = "missing".try_into()?,
            _ => missing.revision_id = "missing".try_into()?,
        };
        cases.push(missing);
    }
    let mut policy = credential(&fixture, "nonprofile")?;
    policy.key = pin.key();
    policy.key.resource_id = "other-policy".try_into()?;
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
    policy.definition = Definition::Policy {
        document: serde_json::to_value(document)?,
        name: "selected".into(),
        dependencies: Vec::new(),
    };
    let result = put(&fixture, fixture.operator.clone(), policy.clone()).await?;
    cases.push(reference(&policy, &result)?);
    let input = request(&fixture, cases)?;
    let observed = read(&fixture, &input)?;
    ensure!(observed.profiles[0].availability == AvailabilityObservation::Eligible);
    for entry in &observed.profiles[1..] {
        ensure!(
            entry.availability == AvailabilityObservation::AuthorityDenied
                && entry.current_profile_revision.is_none()
                && entry.rate.is_none()
                && entry.observed_shared_count.is_none()
                && entry.health == HealthObservation::Unavailable
        );
    }
    fixture.service.shutdown().await?;
    Ok(())
}

#[tokio::test]
async fn scheduled_real_authority_publication_between_sampling_and_finish_rejects_batch()
-> Result<()> {
    let fixture = keyed_fixture().await?;
    let pin = provision(&fixture, "profile", "create", None, rules(2, Some(2), true)).await?;
    let input = request(&fixture, vec![pin.clone()])?;
    let reader = fixture
        .service
        .profile_observation_read(
            &fixture.operator,
            Some(&fixture.installation),
            Instant::now() + Duration::from_secs(5),
        )
        .map_err(failure)?;
    let sampled = reader.sample(&input).map_err(failure)?;
    let guard = fixture
        .service
        .operational
        .try_snapshot()
        .map_err(|e| anyhow::anyhow!("sampler retained lock: {e:?}"))?;
    drop(guard);
    let newer = tokio::time::timeout(
        Duration::from_secs(2),
        provision(
            &fixture,
            "profile",
            "rotate",
            Some(pin.revision_id),
            rules(2, Some(3), true),
        ),
    )
    .await??;
    ensure!(
        matches!(reader.finish(&sampled),Err(error) if error.0.code==ErrorCode::RevisionConflict)
    );
    drop(reader);
    ensure!(
        read(&fixture, &request(&fixture, vec![newer])?)?.profiles[0]
            .rate
            .as_ref()
            .context("rate")?
            .member_rpm
            == 3
    );
    fixture.service.shutdown().await?;
    Ok(())
}
