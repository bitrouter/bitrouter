//! Exercise the shipped assembler's product boundary, never a test-only resource profile.

use std::{collections::BTreeMap, path::Path, time::Duration};

use anyhow::{Context, Result, ensure};
use bitrouter_ai::tenancy::{
    authority::{OwnerKind, ResourceKind},
    configuration::ResourceRef,
};
use sea_orm::{ConnectionTrait, DatabaseBackend, DatabaseConnection, Statement};
use serde_json::{Value, json};

use crate::assemble::{Assembled, build_app};

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
            "database": {"url": format!("sqlite://{}?mode=rwc", root.join("scope.sqlite").display())},
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
    let route = reference(ResourceKind::Route, "scope-model")?;
    Ok(json!({
        "format": 1,
        "installation_id": installation,
        "resources": [
            {"reference": slot, "definition": {"kind": "credential", "credential": {
                "type": "api_key", "data": {"value": "scope-only-provider-secret"}
            }}},
            {"reference": route, "definition": {
                "kind": "route", "selector": "fixture:model-a", "policy": null,
                "routing": {"providers": {"fixture": {
                    "api_base": url, "api_protocol": [{"*": "chat_completions"}],
                    "models": [{"id": "model-a"}]
                }}},
                "credentials": [{"provider": "fixture", "account": "default", "credential": slot}]
            }}
        ]
    }))
}

// Quote every stored scalar rather than comparing counts or decoded summaries.
// This covers the authority header, operations, configurations, resource/lifecycle
// rows and paid receipts/jobs without depending on a historical schema layout.
async fn logical_rows(db: &DatabaseConnection) -> Result<BTreeMap<String, Vec<Vec<String>>>> {
    let tables = db.query_all(Statement::from_string(DatabaseBackend::Sqlite,
        "SELECT name FROM sqlite_schema WHERE type='table' AND name GLOB 'tenancy_*' ORDER BY name COLLATE BINARY"
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
async fn gateway_assembly_rejects_experimental_imports_without_writes() -> Result<()> {
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
    let seed = build_app(&config).await?;
    let service = seed.tenancy.as_ref().context("seed tenancy")?;
    let installation = service
        .current()
        .map_err(super::failure)?
        .installation
        .clone();
    let import = root.join("resources.json");
    let current = manifest(&installation, &format!("{}/v1", upstream.uri()))?;
    config.tenancy.resource_import = Some(import.clone());

    let fields: Vec<(String, serde_json::Value)> = serde_json::from_str(include_str!(
        "../../../tests/fixtures/resource_import/unknown-route-fields.json"
    ))?;
    let mut experimental = Vec::new();
    for (field, value) in fields {
        let mut invalid = current.clone();
        invalid["resources"][1]["definition"][&field] = value;
        let bytes = serde_json::to_vec(&invalid)?;
        let error = service
            .resources
            .provision_bytes(&seed.db, &bytes)
            .await
            .err()
            .with_context(|| format!("resource compiler accepted {field}"))?;
        ensure!(
            error.to_string() == "invalid resource manifest",
            "{error:#}"
        );
        experimental.push((field, bytes));
    }
    close(seed, &root).await?;
    let db = crate::db::connect(&format!(
        "sqlite://{}?mode=ro",
        root.join("scope.sqlite").display()
    ))
    .await?;
    let before = logical_rows(&db).await?;
    for (field, bytes) in experimental {
        std::fs::write(&import, bytes)?;
        let error = build_app(&config)
            .await
            .err()
            .with_context(|| format!("gateway accepted experimental import field {field}"))?;
        // Both compiler ingress and startup redact private resource errors.
        // The valid manifest below proves the injected field causes refusal.
        ensure!(
            format!("{error:#}").contains("native continuation key initialization failed"),
            "{error:#}"
        );
        released(&root).await?;
        assert_eq!(logical_rows(&db).await?, before, "import changed authority");
    }
    ensure!(
        upstream
            .received_requests()
            .await
            .context("request log")?
            .is_empty()
    );

    // The same manifest with the experimental field absent is provisionable.
    std::fs::write(&import, serde_json::to_vec(&current)?)?;
    let assembled = build_app(&config).await?;
    close(assembled, &root).await?;
    let retained = logical_rows(&db).await?;
    ensure!(
        retained["tenancy_resource_revisions"].len()
            == before["tenancy_resource_revisions"].len() + 2,
        "valid import did not commit both resources"
    );
    db.close().await?;
    Ok(())
}
