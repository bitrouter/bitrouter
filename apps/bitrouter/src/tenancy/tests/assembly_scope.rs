//! Exercise the shipped assembler's product boundary, never a test-only resource profile.

use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::{Configuration, ConfigurationRevision, ResourceRef},
};
use bitrouter_ai::types::{ApiProtocol, ModelOperation};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::assemble::{Assembled, AssemblyScope, build_app, build_scoped_app};
use crate::gateway::{
    model::decision::policy::DecisionPolicy,
    routing::blocks::{
        allocation::BanditConfig,
        control::{BlockDefinition, BlockRule},
    },
};

const ROOT: &str = "BITROUTER_SCOPE_ASSEMBLY_ROOT";
const KEY: &str = "BITROUTER_SCOPE_ASSEMBLY_KEY";
const CHILD: &str = "tenancy::tests::assembly_scope::owned_scope_assembly_child";
const LIMIT: Duration = Duration::from_secs(45);

fn config(root: &Path) -> Result<crate::config::Config> {
    crate::config::parse_with(
        &serde_json::to_string(&json!({
            "inherit_defaults": false,
            "registry": {"enabled": false},
            "server": {"listen": "127.0.0.1:0", "skip_auth": false},
            "database": {"url": format!("sqlite://{}", root.join("scope.sqlite").display())},
            "tenancy": {"enabled": true, "wrapping_key_env": KEY},
            "control": {"enabled": true, "listen": "127.0.0.1:0", "credentials": [{
                "id": "scope-operator", "token_env": "BITROUTER_SCOPE_OPERATOR_TOKEN",
                "scopes": ["control:read", "control:tenancy"]
            }]}
        }))?,
        |_| None,
    )
    .map_err(Into::into)
}

fn reference(kind: ResourceKind, name: &str) -> Result<ResourceRef> {
    Ok(ResourceRef {
        owner_kind: OwnerKind::Tenant,
        owner_id: "scope-tenant".try_into()?,
        resource_kind: kind,
        resource_id: name.try_into()?,
        revision_id: "r1".try_into()?,
    })
}

fn manifest(
    installation: &bitrouter_ai::tenancy::identity::InstallationId,
    url: &str,
) -> Result<Value> {
    let slot = reference(ResourceKind::Credential, "scope-slot")?;
    let judge = reference(ResourceKind::Route, "scope-judge")?;
    let route = |reference: ResourceRef, model: &str| {
        json!({"reference": reference, "definition": {
            "kind": "route", "selector": format!("fixture:{model}"), "policy": null,
            "routing": {"providers": {"fixture": {
                "api_base": url, "api_protocol": [{"*": "chat_completions"}],
                "models": [{"id": model}]
            }}},
            "credentials": [{"provider": "fixture", "account": "default", "credential": slot}]
        }})
    };
    let mut subject = route(reference(ResourceKind::Route, "scope-subject")?, "model-a");
    subject["definition"]["assessment"] = json!({"profile": "assessment_v1", "route": judge});
    // The current private Choice writer composes one generation route with a
    // hidden, static Decisions child; its planning policy is not model Policy IR.
    let choice_child = reference(ResourceKind::Route, "scope-choice-child")?;
    let mut choice = route(reference(ResourceKind::Route, "scope-choice")?, "model-a");
    choice["definition"]["core_choice"] = json!({
        "profile": "choice_v1", "route": choice_child, "policy": DecisionPolicy::default()
    });
    let mut child = route(choice_child, "decision-model");
    child["definition"]["routing"]["providers"]["fixture"]["api_protocol"] =
        json!([{"*": "decisions"}]);
    // Match the live capture Block fixture's bounded two-arm definition.
    let definition = BlockDefinition {
        block_id: "scope-block".into(),
        source: "scope-assembly-fixture".into(),
        rationale: "Explicit current two-arm comparison".into(),
        rules: vec![BlockRule {
            selector: "fixture:model-a".into(),
            fingerprint: None,
            baseline_route: "fixture:model-a".into(),
            challenger_route: "fixture:model-b".into(),
        }],
        independence_rationale: "One rule in one independently captured session".into(),
        dependencies: Default::default(),
        measurement_contract: crate::core::evolution::scoring::measurement_contract(
            crate::acp_trajectory::checkpoint::types::AssessmentSource::Human,
            "owned-human/scope-operator",
            "owned-manual-rubric-v1",
        )?,
        batch_sessions: 2,
        bandit: BanditConfig {
            maximum_challenger_sessions: 8,
            maximum_pending_challenger: 8,
            initial_exposure_ppm: 500_000,
            maximum_exposure_ppm: 500_000,
            posterior_draws: 128,
            ..Default::default()
        },
    };
    let mut block = route(reference(ResourceKind::Route, "scope-block")?, "model-a");
    block["definition"]["block"] = json!({"profile": "block_v1", "definition": definition});
    block["definition"]["routing"]["providers"]["fixture"]["models"] = json!([
        {"id": "model-a", "pricing": {"input_micro_usd_per_token": 2.0, "output_micro_usd_per_token": 3.0}},
        {"id": "model-b", "pricing": {"input_micro_usd_per_token": 2.0, "output_micro_usd_per_token": 3.0}}
    ]);
    // Put a valid credential before the rejected route: a partial importer must
    // not leave even that unrelated row or its lifecycle head behind.
    Ok(
        json!({"format": 1, "installation_id": installation, "resources": [
            {"reference": slot, "definition": {"kind": "credential", "credential": {
                "type": "api_key", "data": {"value": "scope-only-provider-secret"}
            }}},
            subject, route(judge, "judge-model"), choice, child, block
        ]}),
    )
}

async fn qualify_current_profiles(
    app: &Assembled,
) -> Result<Vec<crate::tenancy::configuration_store::StoredConfiguration>> {
    let service = app.tenancy.as_ref().context("BRO tenancy")?;
    let mut configurations = Vec::new();
    for (name, dependency_count) in [
        ("scope-subject", 3),
        ("scope-choice", 3),
        ("scope-block", 2),
    ] {
        let document: Configuration = serde_json::from_value(json!({
            "routes": [reference(ResourceKind::Route, name)?], "policy": null,
            "generation_defaults": {}, "learning": {"mode": "off", "max_evaluations": 0},
            "limits": {"active_executions": 1, "queued_work": 1, "learning_jobs": 0}
        }))?;
        let tenant = "scope-tenant".try_into()?;
        let revision = format!("{name}-configuration").try_into()?;
        let compiled = service
            .resources
            .compile(&app.db, &tenant, &revision, &document)
            .await
            .map_err(|code| anyhow::anyhow!("BRO {name} compile: {code:?}"))?;
        ensure!(
            compiled.compiler_version == "tenancy-bro-obligation-v1",
            "wrong {name} compiler"
        );
        ensure!(
            compiled.dependencies.len() == dependency_count
                && compiled
                    .dependencies
                    .iter()
                    .all(|r| r.resource_kind != ResourceKind::Policy),
            "{name} unexpectedly depends on model Policy"
        );
        // Use the real compiler's exact document, dependency closure and sealed
        // payload, as in the current retained-configuration decoder fixtures.
        let mut canonical = serde_json::to_value(&document)?;
        crate::tenancy::command::sort(&mut canonical);
        let stored = crate::tenancy::configuration_store::StoredConfiguration {
            revision: ConfigurationRevision {
                tenant_id: tenant,
                revision_id: revision,
                schema_version: 1,
                compiler_version: compiled.compiler_version,
                content_digest: hex::encode(Sha256::digest(serde_json::to_vec(&canonical)?))
                    .try_into()?,
                document,
            },
            compiled: compiled.payload,
            dependencies: compiled.dependencies,
            creator: "scope-operator".try_into()?,
            authority_revision: service.current().map_err(super::failure)?.revision,
            created_at: "2026-01-01T00:00:00Z".try_into()?,
        };
        let view = service
            .resources
            .open_execution(&stored)
            .map_err(|code| anyhow::anyhow!("BRO {name} execution qualification: {code:?}"))?;
        let root = view
            .select_native(name, ModelOperation::Generation)
            .map_err(|code| anyhow::anyhow!("BRO {name} route: {code:?}"))?;
        match name {
            "scope-choice" => {
                let profile = root
                    .choice_profile()
                    .context("current Choice profile missing")?;
                ensure!(profile.child == reference(ResourceKind::Route, "scope-choice-child")?);
                ensure!(
                    matches!(
                        view.select("scope-choice-child", &ApiProtocol::Decisions),
                        Err(bitrouter_ai::tenancy::operation::ErrorCode::NotFound)
                    ),
                    "Choice child became a visible root"
                );
            }
            "scope-block" => {
                let profile = root
                    .block_profile()
                    .context("current Block profile missing")?;
                ensure!(
                    profile.definition.block_id == "scope-block"
                        && profile.challenger()? == "fixture:model-b"
                );
                ensure!(
                    root.choice_profile().is_none(),
                    "Block gained an unrelated Choice profile"
                );
            }
            _ => {}
        }
        configurations.push(stored);
    }
    Ok(configurations)
}

// Quote every stored scalar rather than comparing counts or decoded summaries.
// This covers the authority header, operations, configurations, resource/lifecycle
// rows and paid receipts/jobs without depending on a historical schema layout.
async fn logical_rows(db: &DatabaseConnection) -> Result<BTreeMap<String, Vec<Vec<String>>>> {
    let tables = db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT name FROM sqlite_schema WHERE type='table' AND (name GLOB 'tenancy_*' OR name IN ('checkpoint_evolution','acp_effective_assessments','acp_assessment_revisions')) ORDER BY name COLLATE BINARY"
    )).await?;
    let mut snapshot = BTreeMap::new();
    for table in tables {
        let name = table.try_get::<String>("", "name")?;
        let identifier = format!("\"{}\"", name.replace('"', "\"\""));
        let columns = db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("PRAGMA table_info({identifier})"),
            ))
            .await?
            .into_iter()
            .map(|row| row.try_get::<String>("", "name"))
            .collect::<std::result::Result<Vec<_>, _>>()?;
        ensure!(!columns.is_empty(), "empty authority table {name}");
        let projection = columns
            .iter()
            .enumerate()
            .map(|(index, column)| {
                format!("quote(\"{}\") AS c{index}", column.replace('"', "\"\""))
            })
            .collect::<Vec<_>>()
            .join(",");
        let mut encoded = Vec::new();
        for row in db
            .query_all(Statement::from_string(
                DatabaseBackend::Sqlite,
                format!("SELECT {projection} FROM {identifier} LIMIT 129"),
            ))
            .await?
        {
            ensure!(encoded.len() < 128, "unbounded scope fixture {name}");
            encoded.push(
                (0..columns.len())
                    .map(|index| row.try_get::<String>("", &format!("c{index}")))
                    .collect::<std::result::Result<Vec<_>, _>>()?,
            );
        }
        encoded.sort();
        snapshot.insert(name, encoded);
    }
    ensure!(snapshot.contains_key("tenancy_installation"));
    ensure!(snapshot.contains_key("tenancy_resource_revisions"));
    Ok(snapshot)
}

async fn released(root: &Path) -> Result<()> {
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            match crate::tenancy::ownership::acquire_path(&root.join("scope.sqlite"), false) {
                Ok(owner) => {
                    drop(owner);
                    return Ok(());
                }
                Err(error)
                    if matches!(
                        error.downcast_ref::<std::fs::TryLockError>(),
                        Some(std::fs::TryLockError::WouldBlock)
                    ) =>
                {
                    tokio::task::yield_now().await
                }
                Err(error) => return Err(error),
            }
        }
    })
    .await
    .context("actual assembled service retained its owner")?
}

async fn close(app: Assembled, root: &Path) -> Result<()> {
    app.tenancy
        .as_ref()
        .context("missing assembled tenancy")?
        .shutdown()
        .await?;
    let db = app.db.clone();
    drop(app);
    db.close().await?;
    released(root).await
}

#[tokio::test]
async fn actual_assembly_accepts_current_bro_profiles_and_gateway_rejects_without_writes()
-> Result<()> {
    let root = tempfile::tempdir()?;
    // Environment secrets belong only to this owned child, never the parallel
    // test runner. The child invokes the real crate-private host assembler.
    let output = tokio::time::timeout(
        LIMIT,
        tokio::process::Command::new(std::env::current_exe()?)
            .args(["--exact", CHILD, "--ignored", "--nocapture"])
            .current_dir(root.path())
            .env(ROOT, root.path())
            .env(KEY, "YWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWFhYWE=")
            .env(
                "BITROUTER_SCOPE_OPERATOR_TOKEN",
                "scope-fixture-operator-token-with-enough-bytes",
            )
            .env("BITROUTER_HOME", root.path().join("home"))
            .kill_on_drop(true)
            .output(),
    )
    .await
    .context("bounded scoped-assembly child timed out")??;
    ensure!(
        output.status.success(),
        "scoped assembly child failed:\n{}\n{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    );
    ensure!(
        String::from_utf8_lossy(&output.stdout).contains("1 passed"),
        "child selector matched no test"
    );
    Ok(())
}

#[tokio::test]
#[ignore = "internal environment-isolated assembler fixture; select its bounded parent"]
async fn owned_scope_assembly_child() -> Result<()> {
    let root = std::path::PathBuf::from(std::env::var_os(ROOT).context("scope fixture root")?);
    let upstream = wiremock::MockServer::start().await;
    let mut config = config(&root)?;
    let config_path = root.join("bitrouter.yaml");
    let seed = build_app(&config).await?;
    let installation = seed
        .tenancy
        .as_ref()
        .context("seed tenancy")?
        .current()
        .map_err(super::failure)?
        .installation
        .clone();
    close(seed, &root).await?;
    let import = root.join("current-profiles.json");
    std::fs::write(
        &import,
        serde_json::to_vec(&manifest(&installation, &format!("{}/v1", upstream.uri()))?)?,
    )?;
    config.tenancy.resource_import = Some(import.clone());
    // Inspect through the supported read-only connector. A writable pool
    // without the configured owner must never reopen a tenant store.
    let db = crate::db::connect(&format!("{}?mode=ro", config.database.url)).await?;
    let before = logical_rows(&db).await?;

    // Startup intentionally reports a generic initialization error. Acceptance
    // of this exact configuration below, with only its code-owned scope changed,
    // distinguishes the product boundary from an invalid fixture.
    ensure!(
        build_app(&config).await.is_err(),
        "Gateway accepted a BRO-only current profile import"
    );
    released(&root).await?;
    assert_eq!(
        logical_rows(&db).await?,
        before,
        "Gateway import changed durable authority"
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("mock request journal")?
            .is_empty()
    );

    let bro = build_scoped_app(
        &config,
        Some(&config_path),
        |_| Ok(()),
        AssemblyScope::BroHarness,
    )
    .await?;
    let configurations = qualify_current_profiles(&bro).await?;
    close(bro, &root).await?;
    let retained = logical_rows(&db).await?;
    ensure!(
        retained != before,
        "BRO assembly did not retain the imported resources"
    );
    std::fs::remove_file(import)?;
    config.tenancy.resource_import = None;

    // Unreferenced, intrinsically valid current BRO resources may remain in the
    // catalog. Ordinary Gateway startup must succeed without granting those
    // resources configuration or execution authority.
    let gateway = build_app(&config).await?;
    let service = gateway.tenancy.as_ref().context("Gateway tenancy")?;
    let before_gateway_calls = logical_rows(&gateway.db).await?;
    for stored in &configurations {
        let revision = &stored.revision;
        service
            .resources
            .validate_compiled(stored)
            .map_err(|code| anyhow::anyhow!("retained current profile integrity: {code:?}"))?;
        ensure!(
            matches!(
                service
                    .resources
                    .compile(
                        &gateway.db,
                        &revision.tenant_id,
                        &revision.revision_id,
                        &revision.document,
                    )
                    .await,
                Err(bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch)
            ),
            "Gateway compiled a BRO-only profile: {}",
            revision.revision_id
        );
        ensure!(
            matches!(
                service.resources.open_execution(stored),
                Err(bitrouter_ai::tenancy::operation::ErrorCode::CapabilityMismatch)
            ),
            "Gateway opened a BRO-only profile for execution: {}",
            revision.revision_id
        );
    }
    assert_eq!(
        logical_rows(&gateway.db).await?,
        before_gateway_calls,
        "Gateway profile qualification changed durable authority"
    );
    ensure!(
        upstream
            .received_requests()
            .await
            .context("mock request journal")?
            .is_empty(),
        "Gateway profile qualification performed provider work"
    );
    close(gateway, &root).await?;
    let reopened = build_scoped_app(
        &config,
        Some(&config_path),
        |_| Ok(()),
        AssemblyScope::BroHarness,
    )
    .await?;
    drop(qualify_current_profiles(&reopened).await?);
    close(reopened, &root).await?;
    ensure!(
        upstream
            .received_requests()
            .await
            .context("mock request journal")?
            .is_empty(),
        "scope validation/import performed provider work"
    );
    db.close().await?;
    Ok(())
}

#[tokio::test]
async fn actual_bro_assembly_refuses_nonloopback_listeners_before_database_creation() -> Result<()>
{
    for control in [false, true] {
        let root = tempfile::tempdir()?;
        let mut config = config(root.path())?;
        if control {
            config.control.listen = "0.0.0.0:0".into();
        } else {
            config.server.listen = "0.0.0.0:0".into();
        }
        let error = build_scoped_app(&config, None, |_| Ok(()), AssemblyScope::BroHarness)
            .await
            .err()
            .context("BRO accepted a nonloopback listener")?;
        ensure!(format!("{error:#}").contains("local BRO harness requires loopback listeners"));
        ensure!(
            !root.path().join("scope.sqlite").exists(),
            "listener validation opened the database"
        );
    }
    Ok(())
}
