//! Actual compiled serializers, exact root association and retained byte limits.
use super::*;
use crate::tenancy::resources::{Envelope, MAX_ARTIFACT_BYTES, execution, manifest::bounded_bytes};

use crate::tenancy::configuration_store::StoredConfiguration;
use crate::tenancy::resources::tests::validation::{reseal, stored};

struct CompiledFixture {
    resources: Resources,
    db: DatabaseConnection,
    row: StoredConfiguration,
    wire: Envelope,
    binding_bytes: usize,
}

async fn compiled_fixture(count: usize, mixed: bool) -> Result<CompiledFixture> {
    let (db, resources) = fixture().await?;
    let mut input = route("profiled", true)?;
    let definition = &mut input["definition"];
    let provider = definition["routing"]["providers"]["fixture"].clone();
    let credential = reference(ResourceKind::Credential, "credential", "v1")?;
    let profile_ref = reference(ResourceKind::Policy, "profile", "v1")?;
    let mut providers = serde_json::Map::new();
    let mut endpoints = Vec::new();
    let mut credentials = Vec::new();
    let mut attachments = Vec::new();
    for index in 0..count {
        let name = format!("provider-{index}");
        providers.insert(name.clone(), provider.clone());
        endpoints.push(json!({"provider":name,"service_id":"model"}));
        credentials.push(json!({"provider":name,"account":"default","credential":credential}));
        for protocol in ["chat_completions", "messages", "responses"] {
            attachments.push(json!({"inbound_protocol":protocol,"candidate_index":index,"credential":credential,"profile":profile_ref}));
        }
    }
    definition["selector"] = json!("ordered");
    definition["routing"]["providers"] = json!(providers);
    definition["routing"]["models"] =
        json!({"ordered":{"strategy":"priority","endpoints":endpoints}});
    definition["credentials"] = json!(credentials);
    definition["attachments"] = json!(attachments);
    let mut imports = vec![super::credential()?, profile("v1")?, input];
    let mut roots = vec![reference(ResourceKind::Route, "route", "profiled")?];
    if mixed {
        for name in ["ordinary-a", "ordinary-b"] {
            let mut ordinary = route("v1", false)?;
            ordinary["reference"]["resource_id"] = json!(name);
            roots.push(serde_json::from_value(ordinary["reference"].clone())?);
            imports.push(ordinary);
        }
    }
    import(&resources, &db, imports).await?;
    let document = serde_json::from_value(json!({"routes":roots,"policy":null,
        "generation_defaults":{"max_output_tokens":25,"temperature":0.0},
        "learning":{"mode":"off","max_evaluations":0},
        "limits":{"active_executions":32,"queued_work":32,"learning_jobs":0}}))?;
    let tenant = "tenant".try_into()?;
    let revision = "config-v1".try_into()?;
    let compiled = resources
        .compile(&db, &tenant, &revision, &document)
        .await
        .map_err(|error| anyhow::anyhow!("compile: {error:?}"))?;
    let binding = (
        &resources.installation,
        &tenant,
        &revision,
        compiled.compiler_version.as_str(),
        &document,
        &compiled.dependencies,
    );
    let binding_bytes = serde_json::to_vec(&binding)?.len();
    let sealed = serde_json::from_value(compiled.payload["sealed"].clone())?;
    let bytes =
        resources
            .key
            .as_ref()
            .context("key")?
            .open_bound("configuration", &binding, &sealed)?;
    let envelope: Envelope = serde_json::from_slice(&bytes)?;
    ensure!(bytes.len() == serde_json::to_vec(&envelope)?.len());
    let row = stored(document, compiled)?;
    resources
        .open_execution(&row)
        .map_err(|code| anyhow::anyhow!("readback: {code:?}"))?;
    Ok(CompiledFixture {
        resources,
        db,
        row,
        wire: envelope,
        binding_bytes,
    })
}

#[tokio::test]
async fn full_profiled_256_compiled_shape_fits_without_raising_any_byte_limit() -> Result<()> {
    let fixture = compiled_fixture(256, false).await?;
    let bytes = bounded_bytes(&fixture.wire, MAX_ARTIFACT_BYTES)?;
    ensure!(fixture.wire.routes.is_empty());
    let root = reference(ResourceKind::Route, "route", "profiled")?;
    let imported = fixture
        .resources
        .load(&fixture.db, &root)
        .await
        .map_err(|code| anyhow::anyhow!("read: {code:?}"))?;
    let artifact_bytes = serde_json::to_vec(&imported)?.len();
    let projection_bytes =
        serde_json::to_vec(imported.envelope.as_ref().context("original projection")?)?.len();
    let mut expanded: Envelope = serde_json::from_slice(&bytes)?;
    expanded.routes = crate::tenancy::resources::compiler::configuration_routes(
        &expanded.document,
        &expanded.artifacts,
    )
    .map_err(|code| anyhow::anyhow!("reconstruction: {code:?}"))?;
    let duplicated_bytes = serde_json::to_vec(&expanded)?.len();
    ensure!(duplicated_bytes > MAX_ARTIFACT_BYTES);
    ensure!(bounded_bytes(&expanded, MAX_ARTIFACT_BYTES).is_err());
    ensure!(expanded.persisted_bytes()? == bytes);
    eprintln!(
        "compiled_256 stored_artifact={artifact_bytes} stored_projection={projection_bytes} old_duplicated={duplicated_bytes} current_plaintext={} binding={}",
        bytes.len(),
        fixture.binding_bytes
    );
    execution::validate(expanded)?;
    let mut oversized: Envelope = serde_json::from_slice(&bytes)?;
    let artifact = oversized
        .artifacts
        .iter_mut()
        .find(|artifact| artifact.reference == root)
        .context("root")?;
    artifact
        .envelope
        .as_mut()
        .context("projection")?
        .system_prompt = Some("x".repeat(MAX_ARTIFACT_BYTES));
    ensure!(oversized.persisted_bytes().is_err());
    Ok(())
}

#[tokio::test]
async fn compact_profiled_readback_never_borrows_missing_duplicate_or_conflicting_authority()
-> Result<()> {
    let fixture = compiled_fixture(2, true).await?;
    let baseline = serde_json::to_value(&fixture.wire)?;
    ensure!(fixture.wire.routes.len() == 2);
    for case in [
        "missing",
        "duplicate_artifact",
        "wrong_revision",
        "wrong_owner",
        "duplicate_projection",
        "credential",
        "profile",
        "ordinal",
        "protocol",
        "continuation",
        "passthrough",
        "ordinary_missing",
        "ordinary_duplicate",
        "ordinary_order",
        "ordinary_default",
    ] {
        let mut changed = baseline.clone();
        let artifact = changed["artifacts"]
            .as_array()
            .context("artifacts")?
            .iter()
            .position(|artifact| artifact["definition"]["kind"] == "profiled_route")
            .context("profiled artifact")?;
        match case {
            "missing" => changed["artifacts"][artifact]["envelope"] = Value::Null,
            "duplicate_artifact" => {
                let duplicate = changed["artifacts"][artifact].clone();
                changed["artifacts"]
                    .as_array_mut()
                    .context("artifacts")?
                    .push(duplicate);
            }
            "wrong_revision" => {
                changed["artifacts"][artifact]["envelope"]["reference"]["revision_id"] =
                    json!("other")
            }
            "wrong_owner" => {
                changed["artifacts"][artifact]["envelope"]["reference"]["owner_id"] = json!("other")
            }
            "duplicate_projection" => {
                let supplied = changed["artifacts"][artifact]["envelope"].clone();
                changed["routes"]
                    .as_array_mut()
                    .context("routes")?
                    .push(supplied);
            }
            "credential" => {
                changed["artifacts"][artifact]["envelope"]["chains"][0]["targets"][0]["credential"]
                    ["revision_id"] = json!("other")
            }
            "profile" => {
                changed["artifacts"][artifact]["definition"]["attachments"][0]["profile"]["revision_id"] =
                    json!("other")
            }
            "ordinal" => {
                changed["artifacts"][artifact]["definition"]["attachments"][0]["candidate_index"] =
                    json!(255)
            }
            "protocol" => {
                changed["artifacts"][artifact]["definition"]["attachments"][0]["inbound_protocol"] =
                    json!("responses")
            }
            "continuation" => {
                changed["artifacts"][artifact]["envelope"]["defaults"]["previous_response_id"] =
                    json!("prior")
            }
            "passthrough" => {
                changed["artifacts"][artifact]["envelope"]["chains"][0]["targets"][0]["target"]["headers"] =
                    json!([{"name":"x-control","default":null,"passthrough":true}])
            }
            "ordinary_missing" => {
                changed["routes"]
                    .as_array_mut()
                    .context("routes")?
                    .remove(0);
            }
            "ordinary_duplicate" => changed["routes"][1] = changed["routes"][0].clone(),
            "ordinary_order" => changed["routes"]
                .as_array_mut()
                .context("routes")?
                .reverse(),
            "ordinary_default" => changed["routes"][0]["defaults"]["temperature"] = json!(9.0),
            _ => anyhow::bail!("unknown mutation"),
        }
        let mut row = fixture.row.clone();
        reseal(&fixture.resources, &mut row, &changed)?;
        ensure!(
            fixture.resources.open_execution(&row).is_err(),
            "accepted {case}"
        );
    }
    Ok(())
}

#[tokio::test]
async fn compact_reconstruction_charges_one_cumulative_shared_budget_before_copying() -> Result<()>
{
    use crate::storage::operation_budget::OperationBudget;
    use crate::tenancy::resources::compiler::configuration_routes_with_operation;
    let fixture = compiled_fixture(2, true).await?;
    let document = &fixture.wire.document;
    let artifacts = &fixture.wire.artifacts;
    let default_bytes = serde_json::to_vec(&(
        "max_output_tokens",
        document.generation_defaults.max_output_tokens,
        "temperature",
        document.generation_defaults.temperature,
    ))?
    .len();
    let charged = document
        .routes
        .iter()
        .map(|reference| {
            let artifact = artifacts
                .iter()
                .find(|artifact| &artifact.reference == reference)
                .context("source")?;
            Ok(
                serde_json::to_vec(artifact.envelope.as_ref().context("projection")?)?.len()
                    + default_bytes,
            )
        })
        .collect::<Result<Vec<usize>>>()?
        .into_iter()
        .sum::<usize>();
    let check = || Ok(());
    let budget = OperationBudget::with_limits(&check, 4096, charged);
    ensure!(configuration_routes_with_operation(document, artifacts, Some(&budget)).is_ok());
    ensure!(configuration_routes_with_operation(document, artifacts, Some(&budget)).is_err());
    let too_small = OperationBudget::with_limits(&check, 4096, charged - 1);
    ensure!(configuration_routes_with_operation(document, artifacts, Some(&too_small)).is_err());
    Ok(())
}
