use super::super::super::super::{COMPILER_VERSION, CompiledConfiguration, Resources};
use super::*;
use crate::tenancy::configuration_store::{self, StoredConfiguration};
use bitrouter_ai::tenancy::{configuration::ConfigurationRevision, identity::RevisionId};
use sea_orm::{ConnectionTrait, DatabaseBackend, Statement};
use sha2::{Digest, Sha256};

fn stored(
    document: Configuration,
    id: RevisionId,
    compiled: CompiledConfiguration,
) -> TestResult<StoredConfiguration> {
    let mut canonical = serde_json::to_value(&document)?;
    crate::tenancy::command::sort(&mut canonical);
    Ok(StoredConfiguration {
        revision: ConfigurationRevision {
            tenant_id: "tenant".try_into()?,
            revision_id: id,
            schema_version: 1,
            compiler_version: compiled.compiler_version,
            content_digest: hex::encode(Sha256::digest(serde_json::to_vec(&canonical)?))
                .try_into()?,
            document,
        },
        compiled: compiled.payload,
        dependencies: compiled.dependencies,
        creator: "principal".try_into()?,
        authority_revision: 1.try_into()?,
        created_at: "2026-01-01T00:00:00Z".try_into()?,
    })
}

#[tokio::test]
async fn authenticated_v1_rows_and_pinned_native_r1_survive_r2_and_source_loss() -> TestResult<()> {
    let db = crate::db::connect("sqlite::memory:").await?;
    crate::db::run_migrations(&db).await?;
    let resources = Resources::with_key("installation".try_into()?, [19; 32])?;
    let original = basic("visible")?;
    let credential = json!({
        "reference":reference(ResourceKind::Credential,"slot")?,
        "definition":{"kind":"credential","credential":{"type":"api_key","data":{"value":"FIXTURE_SECRET"}}}
    });
    resources
        .provision_bytes(
            &db,
            &serde_json::to_vec(&json!({
                "format":1,"installation_id":"installation",
                "resources":[serde_json::to_value(&original)?,credential]
            }))?,
        )
        .await?;
    let doc = document(vec![original.reference.clone()])?;
    let r1: RevisionId = "r1".try_into()?;
    let compiled = checked(
        resources
            .compile(&db, &"tenant".try_into()?, &r1, &doc)
            .await,
    )?;
    assert_eq!(compiled.compiler_version, COMPILER_VERSION);
    let row = stored(doc, r1, compiled)?;
    let before = json!([
        &row.revision,
        &row.compiled,
        &row.dependencies,
        &row.created_at
    ]);
    let artifact_before = configuration_store::resource(&db, &original.reference)
        .await?
        .context("artifact")?;
    checked(resources.validate_compiled(&row))?;
    let first = checked(resources.open_execution(&row))?;
    let selected = checked(first.select_native("visible", ModelOperation::Generation))?;
    let initial = targets(&selected, "visible").await?;
    let initial = initial.first().context("initial target")?;
    assert_eq!(initial.api_protocol, ApiProtocol::Messages);

    let mut newer = original;
    newer.reference.revision_id = "v2".try_into()?;
    if let Definition::Route { routing, .. } = &mut newer.definition {
        routing["providers"]["fixture"]["api_protocol"] =
            json!([{"*":["responses","chat_completions","messages"]}]);
    }
    resources
        .provision_bytes(
            &db,
            &serde_json::to_vec(&json!({
                "format":1,"installation_id":"installation","resources":[&newer]
            }))?,
        )
        .await?;
    let doc = document(vec![newer.reference])?;
    let r2: RevisionId = "r2".try_into()?;
    let compiled = checked(
        resources
            .compile(&db, &"tenant".try_into()?, &r2, &doc)
            .await,
    )?;
    let second = checked(resources.open_execution(&stored(doc, r2, compiled)?))?;
    let newer = checked(second.select_native("visible", ModelOperation::Generation))?;
    let newer = targets(&newer, "visible").await?;
    assert_eq!(
        newer.first().context("new target")?.api_protocol,
        ApiProtocol::Responses
    );
    ensure!(matches!(
        selected.match_candidate(newer.first().context("new target")?),
        Err(ErrorCode::Forbidden)
    ));
    let artifact_after =
        configuration_store::resource(&db, &reference(ResourceKind::Route, "visible")?)
            .await?
            .context("artifact")?;
    assert_eq!(artifact_before.payload, artifact_after.payload);
    assert_eq!(
        artifact_before.content_digest,
        artifact_after.content_digest
    );
    assert_eq!(artifact_before.created_at, artifact_after.created_at);

    db.execute(Statement::from_string(
        DatabaseBackend::Sqlite,
        "DELETE FROM tenancy_resource_revisions".to_owned(),
    ))
    .await?;
    drop(db);
    drop(resources);
    // A cold reader has only the stable key and the retained configuration.
    let reopened = Resources::with_key("installation".try_into()?, [19; 32])?;
    checked(reopened.validate_compiled(&row))?;
    let restarted = checked(reopened.open_execution(&row))?;
    let restarted = checked(restarted.select_native("visible", ModelOperation::Generation))?;
    for retained in [&selected, &restarted] {
        let chain = targets(retained, "visible").await?;
        assert_eq!(
            target::project(chain.first().context("retained target")?)?,
            target::project(initial)?
        );
        assert_eq!(
            checked(retained.match_candidate(initial))?,
            reference(ResourceKind::Credential, "slot")?
        );
    }
    assert_eq!(
        before,
        json!([
            &row.revision,
            &row.compiled,
            &row.dependencies,
            &row.created_at
        ])
    );
    Ok(())
}

/// Exercise the real authenticated reader without requiring mutable import state.
pub(super) fn sealed(
    resources: &Resources,
    envelope: &Envelope,
) -> TestResult<StoredConfiguration> {
    let tenant: bitrouter_ai::tenancy::identity::TenantId = "tenant".try_into()?;
    let revision: RevisionId = "r1".try_into()?;
    let mut dependencies = envelope.document.routes.clone();
    dependencies.extend(envelope.document.policy.iter().cloned());
    for artifact in &envelope.artifacts {
        for dependency in artifact.definition.dependencies() {
            if !dependencies.contains(&dependency) {
                dependencies.push(dependency);
            }
        }
    }
    let sealed = resources.key.as_ref().context("fixture key")?.seal_bound(
        "configuration",
        &(
            &resources.installation,
            &tenant,
            &revision,
            COMPILER_VERSION,
            &envelope.document,
            &dependencies,
        ),
        &serde_json::to_vec(envelope)?,
    )?;
    stored(
        envelope.document.clone(),
        revision,
        CompiledConfiguration {
            compiler_version: COMPILER_VERSION.into(),
            payload: json!({"kind":"sealed","sealed":sealed}),
            dependencies,
        },
    )
}
