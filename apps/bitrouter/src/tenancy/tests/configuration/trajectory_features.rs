//! Current reduced reader consumes guarded drafts from the pinned predecessor.
//! No test builds another artifact or changes this process's environment.
use super::*;
use serde::{Deserialize, Serialize};
use std::{path::Path, sync::Arc, time::Duration};

const KEY: [u8; 32] = [31; 32];
const MODEL: &str = "guarded-route";

#[derive(Serialize, Deserialize)]
struct Transfer {
    installation: InstallationId,
    tenant: TenantId,
    revision: ConfigurationRevision,
    token: String,
}

fn dispatch_headers(service: &Service, transfer: &Transfer) -> Result<::http::HeaderMap> {
    let state = service.current().map_err(failure)?;
    let binding = service
        .capability_document(&state)
        .map_err(|code| anyhow::anyhow!("{code:?}"))?
        .bind(
            transfer.revision.revision_id.clone(),
            "cross-feature".try_into()?,
        )?;
    let value = serde_json::to_value(binding)?;
    let mut headers = ::http::HeaderMap::new();
    for (name, value) in value.as_object().context("binding")? {
        headers.insert(
            ::http::HeaderName::from_bytes(name.as_bytes())?,
            value.as_str().context("header")?.parse()?,
        );
    }
    headers.insert(
        "authorization",
        format!("Bearer {}", transfer.token).parse()?,
    );
    Ok(headers)
}

fn activate(service: &Service, transfer: &Transfer, operation: &str) -> Result<Command> {
    Ok(Command::Activate(
        transfer.tenant.clone(),
        Activation {
            operation_id: operation.try_into()?,
            expected_active_revision: None,
            revision_id: transfer.revision.revision_id.clone(),
            expected_authority_revision: service.current().map_err(failure)?.revision,
            installation_id: transfer.installation.clone(),
        },
    ))
}

#[cfg(not(feature = "core"))]
mod consumer {
    use super::*;
    use crate::tenancy::{ownership, resources::Resources, store};
    use axum::{
        body::Body,
        http::{Request, StatusCode},
    };
    use sea_orm::{ConnectionTrait, DbBackend, Statement};
    use tower::ServiceExt;

    async fn active(db: &sea_orm::DatabaseConnection, tenant: &TenantId) -> Result<String> {
        let row = db
            .query_one(Statement::from_sql_and_values(
                DbBackend::Sqlite,
                "SELECT revision_id FROM tenancy_active_configurations WHERE tenant_id=?",
                [tenant.as_str().into()],
            ))
            .await?
            .context("persisted active pointer")?;
        Ok(row.try_get("", "revision_id")?)
    }

    async fn open(
        path: &Path,
        transfer: &Transfer,
    ) -> Result<(sea_orm::DatabaseConnection, Service)> {
        let owner = ownership::acquire_path(path, false)?;
        let db =
            crate::db::connect_tenancy(&format!("sqlite://{}?mode=rw", path.display()), &owner)
                .await?;
        let started = Service::start_prepared(
            db.clone(),
            Some(owner),
            store::load(&db, Utc::now()).await?,
            Arc::new(Resources::with_key(transfer.installation.clone(), KEY)?),
        )
        .await;
        match started {
            Ok(service) => Ok((db, service)),
            Err(error) => {
                db.close().await?;
                Err(error)
            }
        }
    }

    async fn read(service: &Service, transfer: &Transfer) -> Result<()> {
        let auth = crate::remote_control::auth::ControlAuth::from_lookup(
            &crate::config::ControlConfig {
                credentials: vec![crate::config::ControlCredentialConfig {
                    id: "test-operator".into(),
                    token_env: "CROSS_FEATURE_TEST_OPERATOR".into(),
                    scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
                }],
                ..Default::default()
            },
            |_| Some("cross-feature-operator-at-least-32-bytes".into()),
        )?;
        let response = crate::tenancy::http::router(service.clone(), auth)
            .oneshot(
                Request::get(format!(
                    "/control/v1/tenants/{}/configurations/{}",
                    transfer.tenant, transfer.revision.revision_id
                ))
                .header("authorization", format!("Bearer {}", transfer.token))
                .header("bitrouter-installation-id", transfer.installation.as_str())
                .body(Body::empty())?,
            )
            .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "valid inactive draft read failed"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
        let actual: ConfigurationRevision = serde_json::from_slice(&bytes)?;
        ensure!(
            serde_json::to_value(actual)? == serde_json::to_value(&transfer.revision)?,
            "read changed immutable configuration identity"
        );
        ensure!(service.current().is_ok());
        Ok(())
    }

    async fn consume(directory: &Path) -> Result<()> {
        let transfer: Transfer =
            serde_json::from_slice(&std::fs::read(directory.join("transfer.json"))?)?;
        let (db, service) = open(&directory.join("inactive.db"), &transfer).await?;
        let operator = Caller::Installation(
            crate::tenancy::operator_principal("test-operator").map_err(failure)?,
        );
        ensure!(
            service
                .active_revision(&operator, &transfer.tenant)
                .await
                .map_err(failure)?
                .revision_id
                .is_none()
        );
        read(&service, &transfer).await?;
        // Creation still rejects the exact valid document, without corrupting authority.
        let create = ConfigurationPut {
            operation_id: "unsupported-draft".try_into()?,
            expected_authority_revision: service.current().map_err(failure)?.revision,
            document: transfer.revision.document.clone(),
            installation_id: transfer.installation.clone(),
        };
        ensure!(
            matches!(service.execute(operator.clone(), Command::Configuration(transfer.tenant.clone(), create)).await,
            Err(error) if error.0.code == ErrorCode::CapabilityMismatch)
        );
        ensure!(
            matches!(service.execute(operator.clone(), activate(&service, &transfer, "unsupported-activation")?).await,
            Err(error) if error.0.code == ErrorCode::CapabilityMismatch)
        );
        service
            .dispatch_test_ready
            .store(true, std::sync::atomic::Ordering::Release);
        let headers = dispatch_headers(&service, &transfer)?;
        // Both cold and cached reads must reject before acquiring execution capacity.
        for _ in 0..2 {
            let submission = service.submission(&headers).await.map_err(failure)?;
            ensure!(
                matches!(service.admit_execution(submission, MODEL, &bitrouter_ai::types::ApiProtocol::ChatCompletions).await,
                Err(error) if error.0.code == ErrorCode::CapabilityMismatch)
            );
            ensure!(service.current().is_ok());
        }
        read(&service, &transfer).await?;
        let healthy = ConfigurationPut {
            operation_id: "healthy-empty-draft".try_into()?,
            expected_authority_revision: service.current().map_err(failure)?.revision,
            document: empty_document(),
            installation_id: transfer.installation.clone(),
        };
        retained(
            service
                .execute(
                    operator.clone(),
                    Command::Configuration(transfer.tenant.clone(), healthy),
                )
                .await
                .map_err(failure)?,
        )?;
        // Valid AEAD is insufficient: reconstruction must still reject a changed
        // approved endpoint, even though the public document is untouched.
        let row = crate::tenancy::configuration_store::configuration(
            &db,
            &transfer.tenant,
            &transfer.revision.revision_id,
        )
        .await?
        .context("retained draft")?;
        let key = crate::runtime_crypto::DeploymentKey::from_bytes(KEY)?;
        let binding = (
            &transfer.installation,
            &row.revision.tenant_id,
            &row.revision.revision_id,
            row.revision.compiler_version.as_str(),
            &row.revision.document,
            &row.dependencies,
        );
        let sealed: crate::runtime_crypto::SealedSecret =
            serde_json::from_value(row.compiled["sealed"].clone())?;
        let mut envelope: serde_json::Value =
            serde_json::from_slice(&key.open_bound("configuration", &binding, &sealed)?)?;
        let endpoint = envelope
            .pointer_mut("/routes/0/chains/0/targets/0/target/api_base")
            .context("retained endpoint")?;
        *endpoint = serde_json::json!("https://different.invalid/v1");
        let changed = serde_json::json!({"kind":"sealed", "sealed":key.seal_bound("configuration", &binding, &serde_json::to_vec(&envelope)?)?});
        db.execute(Statement::from_sql_and_values(DbBackend::Sqlite,
            "UPDATE tenancy_configurations SET compiled_json = ? WHERE tenant_id = ? AND revision_id = ?",
            [serde_json::to_string(&changed)?.into(), transfer.tenant.as_str().into(), transfer.revision.revision_id.as_str().into()])).await?;
        ensure!(
            matches!(service.configuration(&operator, &transfer.tenant, &transfer.revision.revision_id).await,
            Err(error) if error.0.code == ErrorCode::StorageUnavailable)
        );
        ensure!(service.current().is_err());
        service.shutdown().await?;
        db.close().await?;
        drop(service);
        // This file was activated by the core producer through the real actor.
        let path = directory.join("active.db");
        let owner = ownership::acquire_path(&path, false)?;
        let db =
            crate::db::connect_tenancy(&format!("sqlite://{}?mode=rw", path.display()), &owner)
                .await?;
        let initial = store::load(&db, Utc::now()).await?;
        ensure!(active(&db, &transfer.tenant).await? == transfer.revision.revision_id.as_str());
        let resources = Resources::with_key(transfer.installation.clone(), KEY)?;
        let row = crate::tenancy::configuration_store::configuration(
            &db,
            &transfer.tenant,
            &transfer.revision.revision_id,
        )
        .await?
        .context("active core configuration")?;
        ensure!(
            resources.validate_compiled(&row).is_ok(),
            "active fixture lost integrity"
        );
        ensure!(resources.validate_executable(&row) == Err(ErrorCode::CapabilityMismatch));
        let started =
            Service::start_prepared(db.clone(), Some(owner), initial, Arc::new(resources)).await;
        match started {
            Ok(service) => {
                service.shutdown().await?;
                anyhow::bail!("unsupported active profile restored");
            }
            Err(error) => ensure!(
                error
                    .to_string()
                    .contains("native continuation key initialization failed")
            ),
        }
        ensure!(active(&db, &transfer.tenant).await? == transfer.revision.revision_id.as_str());
        db.close().await?;
        std::fs::write(
            directory.join("no-core-passed"),
            b"no-core-guarded-read-and-gates-v1",
        )?;
        Ok(())
    }

    #[tokio::test]
    #[ignore = "child of the core guarded-draft transition; requires its exported database"]
    async fn read_core_guarded_draft_without_core() -> Result<()> {
        let directory = std::path::PathBuf::from(
            std::env::var_os("BITROUTER_TEST_GUARDED_TRANSFER")
                .context("core fixture directory")?,
        );
        tokio::time::timeout(Duration::from_secs(30), consume(&directory)).await?
    }
}
