//! Current producer/file-connector and complete v2 restore cutover regressions.
use super::*;
use crate::{offline::recovery::tests::fixture as producer, tenancy::configuration_store};
use anyhow::ensure;
use std::{os::unix::fs::PermissionsExt, path::Path};

fn home() -> Result<tempfile::TempDir> {
    Ok(tempfile::Builder::new()
        .permissions(std::fs::Permissions::from_mode(0o700))
        .tempdir()?)
}

fn options(home: &Path) -> crate::offline::large::Options {
    crate::offline::large::Options {
        working_bytes: 64 * 1024 * 1024,
        scratch_directory: home.to_owned(),
        min_free_bytes: 1,
    }
}

fn image(path: &Path) -> Result<(Vec<u8>, Vec<u8>)> {
    let wal = match std::fs::read(path.with_extension("sqlite-wal")) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => return Err(error.into()),
    };
    Ok((std::fs::read(path)?, wal))
}

async fn current_source(
    home: &Path,
    path: &Path,
) -> Result<(InstallationId, ResourceRef, ResourceRef)> {
    DeploymentKey::from_environment(producer::KEY_ENV)?;
    let live = producer::Live::open(path, None, Some(producer::KEY_ENV)).await?;
    let provisioned = producer::provision(&live).await;
    let closed = live.close(path).await;
    let provisioned = provisioned?;
    closed?;
    let mut manifest = producer::manifest(&provisioned.installation);
    manifest["format"] = json!(2);
    let rows = manifest["resources"]
        .as_array_mut()
        .context("producer resources")?;
    let judge: ResourceRef = serde_json::from_value(rows[1]["reference"].clone())?;
    let mut unused = rows[1].clone();
    unused["reference"]["resource_id"] = json!("zz-unreferenced");
    let unused_reference = serde_json::from_value(unused["reference"].clone())?;
    rows.push(unused);
    for version in [1, 2] {
        let mut profile =
            json!({"schema_version":version,"rate":null,"health":null,"fallback":"stop"});
        if version == 2 {
            profile["health_group_id"] = Value::Null;
        }
        rows.push(json!({"reference":{"owner_kind":"operator_pool","owner_id":"pool","resource_kind":"policy",
            "resource_id":format!("admission-{version}"),"revision_id":"v1"},
            "definition":{"kind":if version == 1 {"admission_profile"} else {"admission_profile_v2"},"profile":profile}}));
    }
    let import = home.join("current-resources.json");
    producer::private_json(&import, &manifest)?;
    let live = producer::Live::open(path, Some(&import), Some(producer::KEY_ENV)).await?;
    live.close(path).await?;
    Ok((provisioned.installation, unused_reference, judge))
}

async fn restore(
    home: &Path,
    source: &Path,
    destination: &Path,
    installation: &InstallationId,
) -> Result<()> {
    let bundle = home.join("bundle");
    let report = crate::offline::snapshot_with_options(source, &bundle, options(home)).await?;
    let report = serde_json::to_value(&report as &dyn crate::output::CliReport)?;
    crate::offline::restore_with_options(
        &bundle,
        report["evidence_digest"]
            .as_str()
            .context("snapshot anchor")?,
        destination,
        installation.as_str(),
        producer::KEY_ENV,
        32,
        options(home),
    )
    .await?;
    Ok(())
}

async fn profile_rows(path: &Path) -> Result<Vec<(String, String, String)>> {
    let database =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}", path.display())).await?;
    let db = database.database();
    let rows = db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT compiler_version,payload_json,content_digest FROM tenancy_resource_revisions WHERE resource_kind='policy' ORDER BY resource_id")).await?;
    let rows = rows
        .into_iter()
        .map(|row| {
            Ok((
                row.try_get("", "compiler_version")?,
                row.try_get("", "payload_json")?,
                row.try_get("", "content_digest")?,
            ))
        })
        .collect::<Result<Vec<_>>>()?;
    db.clone().close().await?;
    drop(database);
    Ok(rows)
}

#[tokio::test]
#[ignore = "requires explicit BITROUTER_TEST_RECOVERY_KEY; actual current file connector and v2 restore"]
async fn independent_admission_profiles_survive_file_restarts_and_v2_restore() -> Result<()> {
    let directory = home()?;
    let home = directory.path().canonicalize()?;
    let source = home.join("current.sqlite");
    let (installation, _, _) = current_source(&home, &source).await?;
    let profiles = profile_rows(&source).await?;
    ensure!(profiles.len() == 2);
    ensure!(profiles[0].0 == PROFILED_COMPILER_VERSION);
    ensure!(profiles[1].0 == SHARED_HEALTH_COMPILER_VERSION);
    for _ in 0..2 {
        let live = producer::Live::open(&source, None, Some(producer::KEY_ENV)).await?;
        live.close(&source).await?;
        ensure!(profile_rows(&source).await? == profiles);
    }
    let original = std::fs::read(&source)?;
    let destination = home.join("restored.sqlite");
    restore(&home, &source, &destination, &installation).await?;
    ensure!(std::fs::read(&source)? == original);
    ensure!(std::fs::read(&destination)? == original);
    ensure!(profile_rows(&destination).await? == profiles);
    let live = producer::Live::open(&destination, None, Some(producer::KEY_ENV)).await?;
    live.close(&destination).await?;
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit BITROUTER_TEST_RECOVERY_KEY; old compiler refuses before writable connector setup"]
async fn retired_policy_compiler_refuses_file_connector_without_writes() -> Result<()> {
    let directory = home()?;
    let home = directory.path().canonicalize()?;
    let source = home.join("retired.sqlite");
    current_source(&home, &source).await?;
    let database =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}", source.display())).await?;
    let changed = database.database().execute(Statement::from_string(DatabaseBackend::Sqlite,
        "UPDATE tenancy_resource_revisions SET compiler_version='tenancy-resources-v1' WHERE resource_id='admission-1' AND resource_kind='policy'")).await?;
    ensure!(changed.rows_affected() == 1);
    database.database().clone().close().await?;
    drop(database);
    let original = image(&source)?;
    ensure!(
        crate::db::connect_owned_tenancy(&format!("sqlite://{}", source.display()))
            .await
            .is_err()
    );
    ensure!(image(&source)? == original);
    Ok(())
}

async fn mutate_authenticated_route(
    path: &Path,
    installation: &InstallationId,
    reference: &ResourceRef,
    retired_exploration: bool,
    current_assessment: Option<ResourceRef>,
) -> Result<()> {
    let database =
        crate::db::connect_owned_tenancy(&format!("sqlite://{}", path.display())).await?;
    let db = database.database();
    let settings = TenancyConfig {
        wrapping_key_env: Some(producer::KEY_ENV.to_owned()),
        ..Default::default()
    };
    let resources = Resources::load_key(installation, &settings)?;
    let mut artifact = resources
        .load(db, reference)
        .await
        .map_err(|code| anyhow::anyhow!("current producer resource unavailable: {code:?}"))?;
    let Definition::Route {
        exploration,
        assessment,
        ..
    } = &mut artifact.definition
    else {
        anyhow::bail!("producer's unreferenced route absent");
    };
    if retired_exploration {
        exploration.push("fixture:retired".to_owned());
    }
    *assessment = current_assessment.map(|route| manifest::AssessmentProfile {
        profile: manifest::AssessmentVersion::V1,
        route,
    });
    let has_assessment = assessment.is_some();
    let import = manifest::Import {
        reference: artifact.reference.clone(),
        definition: artifact.definition.clone(),
    };
    import.validate()?;
    if has_assessment {
        artifact.envelope = Some(compiler::prepare_bro_capture_route(&import, None)?);
    }
    let record = storage::seal(&resources, &artifact)?;
    let changed = db.execute(Statement::from_sql_and_values(DatabaseBackend::Sqlite,
        "UPDATE tenancy_resource_revisions SET compiler_version=?,payload_json=?,dependency_json=?,content_digest=? WHERE resource_id=? AND revision_id=?",
        [record.compiler_version.into(),serde_json::to_string(&record.payload)?.into(),serde_json::to_string(&record.dependencies)?.into(),record.content_digest.as_str().into(),reference.resource_id.as_str().into(),reference.revision_id.as_str().into()])).await?;
    ensure!(changed.rows_affected() == 1);
    ensure!(
        configuration_store::retained_configuration_page(db, None)
            .await?
            .is_empty()
    );
    ensure!(
        lifecycle::exact(db, installation, reference)
            .await?
            .is_some()
    );
    db.clone().close().await?;
    drop(database);
    Ok(())
}

#[tokio::test]
#[ignore = "requires explicit BITROUTER_TEST_RECOVERY_KEY; authenticated unreferenced resource startup and v2 restore"]
async fn unreferenced_retired_exploration_refuses_startup_and_v2_restore_without_writes()
-> Result<()> {
    let directory = home()?;
    let home = directory.path().canonicalize()?;
    let source = home.join("retired-route.sqlite");
    let (installation, reference, _) = current_source(&home, &source).await?;
    mutate_authenticated_route(&source, &installation, &reference, true, None).await?;
    let original = image(&source)?;
    match producer::Live::open(&source, None, Some(producer::KEY_ENV)).await {
        Ok(live) => {
            live.close(&source).await?;
            anyhow::bail!("retired unreferenced route reached a live listener");
        }
        Err(_) => ensure!(image(&source)? == original),
    }
    let destination = home.join("must-not-exist.sqlite");
    ensure!(
        restore(&home, &source, &destination, &installation)
            .await
            .is_err()
    );
    ensure!(!destination.exists());
    ensure!(image(&source)? == original);
    Ok(())
}

#[cfg(all(feature = "core", feature = "harness"))]
#[tokio::test]
#[ignore = "requires explicit BITROUTER_TEST_RECOVERY_KEY; current BRO assessment integrity stays readable"]
async fn unreferenced_current_bro_assessment_integrity_survives_v2_restore() -> Result<()> {
    let directory = home()?;
    let home = directory.path().canonicalize()?;
    let source = home.join("current-bro.sqlite");
    let (installation, reference, judge) = current_source(&home, &source).await?;
    mutate_authenticated_route(&source, &installation, &reference, false, Some(judge)).await?;
    let original = std::fs::read(&source)?;
    let destination = home.join("restored.sqlite");
    restore(&home, &source, &destination, &installation).await?;
    ensure!(std::fs::read(&source)? == original);
    ensure!(std::fs::read(&destination)? == original);
    Ok(())
}

#[tokio::test]
async fn admission_compiler_domain_cannot_authenticate_a_model_policy_definition() -> Result<()> {
    let (_, resources) = fixture().await?;
    let input: manifest::Import = serde_json::from_value(policy("tenant", "model-policy")?)?;
    let artifact = Artifact {
        reference: input.reference,
        definition: input.definition,
        envelope: None,
    };
    let mut record = storage::seal(&resources, &artifact)?;
    let key = resources.key.as_ref().context("test key")?;
    for compiler in [PROFILED_COMPILER_VERSION, SHARED_HEALTH_COMPILER_VERSION] {
        let dependencies = artifact.definition.dependencies();
        let binding = (
            &resources.installation,
            &artifact.reference,
            compiler,
            &dependencies,
        );
        record.compiler_version = compiler.to_owned();
        record.payload = serde_json::to_value(key.seal_bound(
            "resource",
            &binding,
            &serde_json::to_vec(&artifact)?,
        )?)?;
        record.content_digest = key
            .fingerprint(
                "tenancy-resource-v1",
                "immutable-definition",
                &manifest::bounded_bytes(&(&binding, &artifact), 2 * MAX_ARTIFACT_BYTES)?,
            )?
            .try_into()?;
        ensure!(storage::authenticate(&resources, record.clone()).is_err());
        record.payload = json!({"kind":"policy","document":{"lockfileVersion":4}});
        ensure!(storage::authenticate(&resources, record.clone()).is_err());
    }
    Ok(())
}

#[tokio::test]
async fn standalone_authentication_retires_exploration_but_preserves_current_bro_integrity()
-> Result<()> {
    let (_, resources) = fixture().await?;
    let mut value = route("tenant", "unreferenced", "model")?;
    value["definition"]["exploration"] = json!(["fixture:retired"]);
    let input: manifest::Import = serde_json::from_value(value.clone())?;
    input.validate()?;
    let artifact = Artifact {
        reference: input.reference,
        definition: input.definition,
        envelope: None,
    };
    let error = storage::authenticate(&resources, storage::seal(&resources, &artifact)?)
        .err()
        .context("authenticated retired route was accepted")?;
    ensure!(error.to_string().contains("retired route exploration"));

    value["definition"]["exploration"] = json!([]);
    value["definition"]["assessment"] =
        json!({"profile":"assessment_v1","route":reference("tenant",ResourceKind::Route,"judge")?});
    let input: manifest::Import = serde_json::from_value(value)?;
    input.validate()?;
    ensure!(input.definition.require_service().is_err());
    let artifact = Artifact {
        reference: input.reference,
        definition: input.definition,
        envelope: None,
    };
    let current = storage::authenticate(&resources, storage::seal(&resources, &artifact)?)?;
    current.definition.validate_current_profile()?;
    ensure!(current.definition.require_service().is_err());
    Ok(())
}
