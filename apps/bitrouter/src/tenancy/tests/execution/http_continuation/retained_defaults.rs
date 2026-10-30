//! Simulate a compiler-v1 revision sealed before reserved-default qualification.
//! The operator key is a fixture input; no production authority API is exposed.
use super::*;
use crate::tenancy::{Service, command::Command, configuration_store, resources::Resources};
use bitrouter_ai::tenancy::{configuration::Activation, operation::ErrorCode};
use sea_orm::sea_query::{Alias, Expr, Query};

async fn replace(ready: &Ready, compiled: &Value) -> Result<()> {
    let update = Query::update()
        .table(Alias::new("tenancy_configurations"))
        .value(
            Alias::new("compiled_json"),
            serde_json::to_string(compiled)?,
        )
        .and_where(Expr::col(Alias::new("tenant_id")).eq(ready.fixture.tenant.as_str()))
        .and_where(Expr::col(Alias::new("revision_id")).eq(ready.revision.revision_id.as_str()))
        .to_owned();
    ensure!(
        ready
            .fixture
            .db
            .execute(ready.fixture.db.get_database_backend().build(&update))
            .await?
            .rows_affected()
            == 1
    );
    Ok(())
}

async fn restart(ready: &Ready) -> Result<Service> {
    Service::start_prepared(
        ready.fixture.db.clone(),
        None,
        Default::default(),
        crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now()).await?,
        Arc::new(Resources::with_key(
            ready.fixture.installation.clone(),
            [29; 32],
        )?),
    )
    .await
}

fn activation(ready: &Ready, operation: &str) -> Result<Command> {
    Ok(Command::Activate(
        ready.fixture.tenant.clone(),
        Activation {
            operation_id: operation.try_into()?,
            expected_active_revision: None,
            revision_id: ready.revision.revision_id.clone(),
            expected_authority_revision: ready.fixture.service.current().map_err(failure)?.revision,
            installation_id: ready.fixture.installation.clone(),
        },
    ))
}

fn historic_payload(
    ready: &Ready,
    row: &configuration_store::StoredConfiguration,
    field: &str,
) -> Result<Value> {
    let key = crate::runtime_crypto::DeploymentKey::from_bytes([29; 32])?;
    let binding = (
        &ready.fixture.installation,
        &row.revision.tenant_id,
        &row.revision.revision_id,
        row.revision.compiler_version.as_str(),
        &row.revision.document,
        &row.dependencies,
    );
    let sealed = serde_json::from_value(row.compiled["sealed"].clone())?;
    let mut envelope: Value =
        serde_json::from_slice(&key.open_bound("configuration", &binding, &sealed)?)?;
    let reference = serde_json::to_value(&ready.route)?;
    let artifacts = envelope["artifacts"]
        .as_array_mut()
        .context("retained artifacts")?;
    let artifact = artifacts
        .iter_mut()
        .find(|a| a["reference"] == reference)
        .context("route artifact")?;
    let router = artifact
        .pointer_mut("/definition/routing/routers/owned")
        .context("source router")?;
    router
        .pointer_mut("/defaults/params")
        .and_then(Value::as_object_mut)
        .context("source defaults")?
        .insert(field.into(), "foreign-provider-state".into());
    artifact["envelope"]["defaults"][field] = "foreign-provider-state".into();
    let routes = envelope["routes"]
        .as_array_mut()
        .context("compiled routes")?;
    let route = routes
        .iter_mut()
        .find(|r| r["reference"] == reference)
        .context("selected route")?;
    route["defaults"][field] = "foreign-provider-state".into();
    Ok(
        json!({"kind":"sealed", "sealed":key.seal_bound("configuration", &binding, &serde_json::to_vec(&envelope)?)?}),
    )
}

async fn read_http(ready: &Ready) -> Result<()> {
    let auth = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig {
            credentials: vec![crate::config::ControlCredentialConfig {
                id: "test-operator".into(),
                token_env: "HTTP034_TEST_OPERATOR".into(),
                scopes: vec![bitrouter_ai::control::ControlScope::Tenancy],
            }],
            ..Default::default()
        },
        |_| Some("http034-operator-token-at-least-32-bytes".into()),
    )?;
    let mut request = Request::get(format!(
        "/control/v1/tenants/{}/configurations/{}",
        ready.fixture.tenant, ready.revision.revision_id,
    ))
    .body(Body::empty())?;
    *request.headers_mut() = ready.headers.clone();
    let response = crate::tenancy::http::router(ready.fixture.service.clone(), auth)
        .oneshot(request)
        .await?;
    ensure!(
        response.status() == StatusCode::OK,
        "integrity-valid inactive revision must stay readable"
    );
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024).await?;
    ensure!(serde_json::from_slice::<Value>(&bytes)? == serde_json::to_value(&ready.revision)?);
    ensure!(ready.fixture.service.current().is_ok());
    Ok(())
}

#[tokio::test]
async fn sealed_reserved_defaults_remain_readable_but_cannot_execute_or_activate() -> Result<()> {
    for field in ["previous_response_id", "conversation"] {
        let mut provider = Server::start().await?;
        let mut ready = ready(&provider).await?;
        let good = draft(
            &ready.fixture,
            &ready.route,
            "unmodified-control-revision",
            32,
        )
        .await?;
        ready.fixture.service.shutdown().await?;
        let mut row = configuration_store::configuration(
            &ready.fixture.db,
            &ready.fixture.tenant,
            &ready.revision.revision_id,
        )
        .await?
        .context("stored revision")?;
        row.compiled = historic_payload(&ready, &row, field)?;
        let resources = Resources::with_key(ready.fixture.installation.clone(), [29; 32])?;
        ensure!(
            resources.validate_compiled(&row).is_ok(),
            "fixture must reconstruct, not merely authenticate"
        );
        ensure!(resources.validate_executable(&row) == Err(ErrorCode::InvalidRequest));
        replace(&ready, &row.compiled).await?;
        ready.fixture.service = restart(&ready).await?;
        ready
            .fixture
            .service
            .dispatch_test_ready
            .store(true, Ordering::Release);
        let issued = ready.fixture.issue("historic-default-caller").await?;
        let token = issued.secret.as_deref().context("issued secret")?;
        ready.headers = headers(&ready.fixture, token, &ready.revision.revision_id)?;
        let outcome: Result<()> = async {
            read_http(&ready).await?;
            // Both cold and cached retained reads, and both ingress wire kinds,
            // reject before rendering can smuggle a provider's raw identifier.
            for _ in 0..2 {
                for (path, input) in [
                    ("/v1/responses", body(false, None)),
                    ("/v1/chat/completions", json!({"model":MODEL,"messages":[{"role":"user","content":"fresh"}]})),
                ] {
                    let (status, bytes) = send_at(&ready, ready.headers.clone(), path, input).await?;
                    ensure!(!status.is_success());
                    let error: bitrouter_ai::tenancy::operation::Error = serde_json::from_slice(&bytes)?;
                    ensure!(error.code == ErrorCode::InvalidRequest);
                    ensure!(ready.fixture.service.current().is_ok());
                }
            }
            ensure!(matches!(ready.fixture.service.execute(ready.fixture.operator.clone(), activation(&ready, "reject-historic-activation")?).await, Err(error) if error.0.code == ErrorCode::InvalidRequest));
            ensure!(provider.state.calls.load(Ordering::SeqCst) == 0);
            ensure!(row_count(&ready).await? == 0);
            read_http(&ready).await?;
            let good_headers = headers(&ready.fixture, token, &good.revision_id)?;
            let (status, bytes) = send(&ready, good_headers, body(false, None)).await?;
            ensure!(status == StatusCode::OK);
            public_id(&bytes, false)?;
            ensure!(provider.wire().await?.get(field).is_none());
            Ok(())
        }.await;
        ready.fixture.service.shutdown().await?;
        provider.close().await?;
        outcome?;
    }
    Ok(())
}

#[tokio::test]
async fn sealed_active_reserved_defaults_refuse_startup_without_rewriting_rows() -> Result<()> {
    for field in ["previous_response_id", "conversation"] {
        let provider = Server::start().await?;
        let mut ready = ready(&provider).await?;
        ready
            .fixture
            .service
            .execute(
                ready.fixture.operator.clone(),
                activation(&ready, "activate-before-historical-import")?,
            )
            .await
            .map_err(failure)?;
        ready.fixture.service.shutdown().await?;
        let row = configuration_store::configuration(
            &ready.fixture.db,
            &ready.fixture.tenant,
            &ready.revision.revision_id,
        )
        .await?
        .context("active revision")?;
        let changed = historic_payload(&ready, &row, field)?;
        replace(&ready, &changed).await?;
        let outcome: Result<()> = async {
            match restart(&ready).await {
                Ok(service) => {
                    service.shutdown().await?;
                    anyhow::bail!("active reserved defaults were restored");
                }
                Err(error) => ensure!(
                    error
                        .to_string()
                        .contains("native continuation key initialization failed")
                ),
            }
            let actual = configuration_store::configuration(
                &ready.fixture.db,
                &ready.fixture.tenant,
                &ready.revision.revision_id,
            )
            .await?
            .context("unchanged active revision")?;
            ensure!(actual.compiled == changed);
            ensure!(
                crate::tenancy::store::load(&ready.fixture.db, chrono::Utc::now())
                    .await?
                    .active_configurations
                    .get(&ready.fixture.tenant)
                    == Some(&ready.revision.revision_id)
            );
            ensure!(provider.state.calls.load(Ordering::SeqCst) == 0);
            // Known startup rejection released custody; restoration can retry.
            replace(&ready, &row.compiled).await?;
            ready.fixture.service = restart(&ready).await?;
            ready.fixture.service.shutdown().await?;
            Ok(())
        }
        .await;
        provider.close().await?;
        outcome?;
    }
    Ok(())
}
