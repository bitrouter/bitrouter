use super::*;
use serde_json::{Value, json};

fn source(model: &str) -> Value {
    json!({"spec_version":"policy/1.0.0-beta.1", "policies":{"auto":{
        "tiers":{"default":{"model":model}}, "default_tier":"default"
    }}})
}

fn input(fixture: &Fixture, operation: &str, model: &str) -> Result<ResourcePut> {
    let mut input = credential(fixture, operation)?;
    input.key.resource_kind = ResourceKind::Policy;
    input.key.resource_id = "auto-policy".try_into()?;
    input.definition = Definition::Policy {
        document: source(model),
        name: "auto".into(),
        dependencies: Vec::new(),
    };
    Ok(input)
}

#[tokio::test]
async fn source_reads_remain_revision_exact_after_rotation_and_restart() -> Result<()> {
    let mut fixture = keyed_fixture().await?;
    let first = input(&fixture, "policy-create", "mock:small")?;
    let created = put(&fixture, fixture.operator.clone(), first.clone()).await?;
    let original = reference(&first, &created)?;
    let replay = put(&fixture, fixture.operator.clone(), first.clone()).await?;
    ensure!(replay.resource_revision == created.resource_revision);
    let mut second = input(&fixture, "policy-rotate", "mock:large")?;
    second.expected_revision = Some(original.revision_id.clone());
    let rotated = put(&fixture, fixture.operator.clone(), second.clone()).await?;
    let current = reference(&second, &rotated)?;
    restart(&mut fixture).await?;
    for (reference, expected) in [
        (&original, source("mock:small")),
        (&current, source("mock:large")),
    ] {
        let read = fixture
            .service
            .policy_source(&fixture.operator, &fixture.tenant, reference)
            .await
            .map_err(failure)?;
        ensure!(&read.status.reference == reference);
        ensure!(read.status.head_revision.as_ref() == Some(&current.revision_id));
        ensure!(serde_json::to_value(read.document)? == expected);
        ensure!(read.name == "auto");
    }
    let secret = fixture.issue("policy-read-key").await?;
    let caller = fixture
        .service
        .authenticate(secret.secret.as_deref().context("key")?)
        .await
        .map_err(failure)?;
    fixture
        .service
        .policy_source(&caller, &fixture.tenant, &original)
        .await
        .map_err(failure)?;
    let mut other = original.clone();
    other.owner_id = "tenant-other".try_into()?;
    ensure!(
        fixture
            .service
            .policy_source(&caller, &fixture.tenant, &other)
            .await
            .is_err()
    );
    Ok(())
}

#[tokio::test]
async fn legacy_and_experimental_source_rejection_does_not_create_a_resource_head() -> Result<()> {
    let fixture = keyed_fixture().await?;
    let mut invalid = input(&fixture, "bad-policy", "mock:small")?;
    for version in [
        "policy/1.0.0-beta.2",
        "registry/1.0.0-beta.1",
        "1.0.0-beta.1",
    ] {
        let mut document = source("mock:small");
        document["spec_version"] = json!(version);
        invalid.definition = Definition::Policy {
            document,
            name: "auto".into(),
            dependencies: Vec::new(),
        };
        ensure!(
            fixture
                .service
                .execute(
                    fixture.operator.clone(),
                    Command::Resource(fixture.tenant.clone(), invalid.clone())
                )
                .await
                .is_err()
        );
    }
    invalid.definition = Definition::Policy {
        document: serde_json::to_value(crate::policy_lock::PolicyLock::default())?,
        name: "auto".into(),
        dependencies: Vec::new(),
    };
    ensure!(
        fixture
            .service
            .execute(
                fixture.operator.clone(),
                Command::Resource(fixture.tenant.clone(), invalid)
            )
            .await
            .is_err()
    );
    let valid = input(&fixture, "valid-policy", "mock:small")?;
    put(&fixture, fixture.operator.clone(), valid).await?;
    Ok(())
}
