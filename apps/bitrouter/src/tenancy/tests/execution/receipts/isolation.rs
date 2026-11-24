//! Corrupt retained content must fail locally while the same Core keeps serving.
use super::*;
use crate::tenancy::{Service, command::Command};
use bitrouter_ai::tenancy::{
    authority::{BindingPut, GrantScope, TenantPut, TenantRole, TenantState},
    configuration::Activation,
    identity::TenantId,
};
use sea_orm::{ConnectionTrait, Statement};

#[derive(Clone, Copy, Debug)]
enum Corruption {
    Credential,
    Route,
    Configuration,
    ActivePointer,
}

async fn second_tenant(first: &Fixture) -> Result<Fixture> {
    let tenant: TenantId = "tenant-b".try_into()?;
    let response = first
        .service
        .execute(
            first.operator.clone(),
            Command::Tenant(
                tenant.clone(),
                TenantPut {
                    operation_id: "isolation-second-tenant".try_into()?,
                    expected_revision: None,
                    state: TenantState::Active,
                    installation_id: first.installation.clone(),
                },
            ),
        )
        .await
        .map_err(failure)?;
    crate::tenancy::tests::result(response)?;
    let binding_result = crate::tenancy::tests::result(
        first
            .service
            .execute(
                first.operator.clone(),
                Command::Binding(
                    tenant.clone(),
                    first.binding.clone(),
                    BindingPut {
                        operation_id: "isolation-second-binding".try_into()?,
                        expected_revision: None,
                        principal_id: first.principal.clone(),
                        role: TenantRole::TenantAdmin,
                        scope: GrantScope::Tenant {
                            operator_pools: vec![],
                        },
                        expires_at: crate::tenancy::tests::expiry()?,
                        expected_management_owner: None,
                        installation_id: first.installation.clone(),
                    },
                ),
            )
            .await
            .map_err(failure)?,
    )?;
    Ok(Fixture {
        wrapping_key: first.wrapping_key,
        service: first.service.clone(),
        db: first.db.clone(),
        operator: first.operator.clone(),
        installation: first.installation.clone(),
        tenant,
        principal: first.principal.clone(),
        binding: first.binding.clone(),
        binding_result,
    })
}

fn tamper_ciphertext(stored: &str, path: &str) -> Result<String> {
    let mut envelope: Value = serde_json::from_str(stored)?;
    let ciphertext = envelope.pointer_mut(path).context("sealed ciphertext")?;
    let mut bytes = ciphertext
        .as_str()
        .context("ciphertext text")?
        .as_bytes()
        .to_vec();
    let first = bytes.first_mut().context("nonempty ciphertext")?;
    *first = if *first == b'A' { b'B' } else { b'A' };
    // Keep the envelope and base64 syntactically valid and the same size, so
    // refusal proves authentication rather than a transfer/format size limit.
    *ciphertext = Value::String(String::from_utf8(bytes)?);
    Ok(serde_json::to_string(&envelope)?)
}

async fn corrupt(harness: &Harness, corruption: Corruption) -> Result<()> {
    let db = &harness.fixture.db;
    if matches!(corruption, Corruption::ActivePointer) {
        // The stopped fixture intentionally violates the active-target FK.
        // PRAGMAs are connection-local: restore enforcement on this same
        // physical connection even when the injected mutation fails.
        let mut connection = db.get_sqlite_connection_pool().acquire().await?;
        sea_orm::sqlx::query("PRAGMA foreign_keys=OFF")
            .execute(&mut *connection)
            .await?;
        let changed = sea_orm::sqlx::query(
            "UPDATE tenancy_active_configurations SET revision_id='missing-revision' WHERE tenant_id=?",
        )
        .bind(harness.fixture.tenant.as_str())
        .execute(&mut *connection)
        .await;
        sea_orm::sqlx::query("PRAGMA foreign_keys=ON")
            .execute(&mut *connection)
            .await?;
        let enabled: i64 = sea_orm::sqlx::query_scalar("PRAGMA foreign_keys")
            .fetch_one(&mut *connection)
            .await?;
        ensure!(
            enabled == 1,
            "fixture did not restore foreign-key enforcement"
        );
        ensure!(changed?.rows_affected() == 1);
        return Ok(());
    }
    let backend = db.get_database_backend();
    let statement = match corruption {
        Corruption::Credential | Corruption::Route => {
            let reference = if matches!(corruption, Corruption::Credential) {
                &harness.slot
            } else {
                &harness.route
            };
            let kind = serde_json::to_value(reference.resource_kind)?;
            let keys: Vec<sea_orm::Value> = vec![
                reference.owner_id.as_str().into(),
                kind.as_str().context("resource kind")?.into(),
                reference.resource_id.as_str().into(),
                reference.revision_id.as_str().into(),
            ];
            let row = db
                .query_one(Statement::from_sql_and_values(
                    backend,
                    "SELECT payload_json FROM tenancy_resource_revisions WHERE owner_kind='tenant' AND owner_id=? AND resource_kind=? AND resource_id=? AND revision_id=?",
                    keys.clone(),
                ))
                .await?
                .context("retained resource")?;
            let payload =
                tamper_ciphertext(&row.try_get::<String>("", "payload_json")?, "/ciphertext")?;
            let mut values = vec![payload.into()];
            values.extend(keys);
            Statement::from_sql_and_values(
                backend,
                "UPDATE tenancy_resource_revisions SET payload_json=? WHERE owner_kind='tenant' AND owner_id=? AND resource_kind=? AND resource_id=? AND revision_id=?",
                values,
            )
        }
        Corruption::Configuration => {
            let keys: Vec<sea_orm::Value> = vec![
                harness.fixture.tenant.as_str().into(),
                harness.revision.revision_id.as_str().into(),
            ];
            let row = db
                .query_one(Statement::from_sql_and_values(
                    backend,
                    "SELECT compiled_json FROM tenancy_configurations WHERE tenant_id=? AND revision_id=?",
                    keys.clone(),
                ))
                .await?
                .context("retained configuration")?;
            let payload = tamper_ciphertext(
                &row.try_get::<String>("", "compiled_json")?,
                "/sealed/ciphertext",
            )?;
            let mut values = vec![payload.into()];
            values.extend(keys);
            Statement::from_sql_and_values(
                backend,
                "UPDATE tenancy_configurations SET compiled_json=? WHERE tenant_id=? AND revision_id=?",
                values,
            )
        }
        Corruption::ActivePointer => {
            anyhow::bail!("active-pointer fixture must own its SQLite connection")
        }
    };
    ensure!(db.execute(statement).await?.rows_affected() == 1);
    Ok(())
}

async fn cold_restart(harness: &mut Harness, second: &mut Fixture) -> Result<()> {
    harness._gateway.abort();
    harness.fixture.service.shutdown().await?;
    // A fresh Resources and Service discard all authenticated configuration and
    // credential caches, while retaining both tenants in the same database.
    harness.fixture.service = Service::start_prepared(
        harness.fixture.db.clone(),
        None,
        crate::tenancy::store::initial_header(&harness.fixture.db, chrono::Utc::now()).await?,
        Arc::new(crate::tenancy::resources::Resources::with_key(
            harness.fixture.installation.clone(),
            harness.fixture.wrapping_key,
        )?),
    )
    .await?;
    second.service = harness.fixture.service.clone();
    harness
        .fixture
        .service
        .dispatch_test_ready
        .store(true, Ordering::Release);
    let operators = crate::remote_control::auth::ControlAuth::from_lookup(
        &crate::config::ControlConfig::default(),
        |_| Some("receipt-fixture-operator-token-at-least-32-bytes".into()),
    )?;
    let dispatch = Router::new()
        .route(harness.path, post(|| async { StatusCode::NOT_FOUND }))
        .layer(axum::middleware::from_fn_with_state(
            harness.fixture.service.clone(),
            crate::tenancy::http::dispatch,
        ));
    let router =
        crate::tenancy::http::router(harness.fixture.service.clone(), operators).merge(dispatch);
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    harness.base = format!("http://{}", listener.local_addr()?);
    harness._gateway =
        AbortOnDropHandle::new(tokio::spawn(
            async move { axum::serve(listener, router).await },
        ));
    harness.headers = headers(
        &harness.fixture,
        harness
            .credential
            .secret
            .as_deref()
            .context("broken tenant token")?,
        &harness.revision.revision_id,
    )?;
    Ok(())
}

fn live(harness: &Harness) -> Result<()> {
    ensure!(harness.fixture.service.live.load(Ordering::Acquire));
    ensure!(harness.fixture.service.current().is_ok());
    ensure!(harness.fixture.service.admission.receipts_ready());
    Ok(())
}

async fn harness() -> Result<Harness> {
    Harness::with_namespace_resources(
        "chat_completions",
        StatusCode::OK,
        serde_json::to_string(&json_response(
            "chat_completions",
            full_usage("chat_completions"),
        )?)?,
        false,
        None,
        true,
    )
    .await
}

async fn check(corruption: Corruption) -> Result<()> {
    let mut harness = harness().await?;
    let checked: Result<()> = async {
        let mut second = second_tenant(&harness.fixture).await?;
        let mut route = harness.route.clone();
        route.owner_id = second.tenant.as_str().try_into()?;
        let revision = draft(&second, &route, "isolation-healthy-config", 25).await?;
        let credential = second.issue("isolation-healthy-client").await?;
        if matches!(corruption, Corruption::ActivePointer) {
            let activation = Activation {
                operation_id: "isolation-activate".try_into()?,
                expected_active_revision: None,
                revision_id: harness.revision.revision_id.clone(),
                expected_authority_revision: harness
                    .fixture
                    .service
                    .current()
                    .map_err(failure)?
                    .revision,
                installation_id: harness.fixture.installation.clone(),
            };
            let response = harness
                .fixture
                .service
                .execute(
                    harness.fixture.operator.clone(),
                    Command::Activate(harness.fixture.tenant.clone(), activation),
                )
                .await
                .map_err(failure)?;
            crate::tenancy::tests::result(response)?;
        }
        ensure!(harness.calls.load(Ordering::SeqCst) == 0);
        // Corrupt the stopped store, then prove startup succeeds without loading
        // this retained content into a cache or touching the provider.
        harness.fixture.service.shutdown().await?;
        corrupt(&harness, corruption).await?;
        cold_restart(&mut harness, &mut second).await?;
        live(&harness)?;
        let client = Harness::client(false)?;
        let broken_token = harness
            .credential
            .secret
            .as_deref()
            .context("broken token")?;
        let healthy_token = credential.secret.as_deref().context("healthy token")?;
        let healthy_headers = headers(&second, healthy_token, &revision.revision_id)?;
        let mut failed_id = None;
        if matches!(
            corruption,
            Corruption::Credential | Corruption::Configuration
        ) {
            let mut request = harness.request.clone();
            request["messages"][0]["content"] = json!("broken-tenant-private");
            let response = client
                .post(format!("{}{}", harness.base, harness.path))
                .headers(harness.headers.clone())
                .json(&request)
                .send()
                .await?;
            ensure!(
                response.status() == StatusCode::SERVICE_UNAVAILABLE,
                "corrupt inference status: {}",
                response.status()
            );
            if response.headers().contains_key("x-bitrouter-request-id") {
                failed_id = Some(execution_id(&response)?);
            }
            let error = response.text().await?;
            ensure!(
                !error.contains("owned-upstream-secret")
                    && !error.contains("ciphertext")
                    && !error.contains(healthy_token)
                    && !error.contains("broken-tenant-private")
            );
            live(&harness)?;
        }
        let read_path = match corruption {
            Corruption::ActivePointer => Some(format!(
                "/control/v1/tenants/{}/activation",
                harness.fixture.tenant
            )),
            Corruption::Configuration => Some(format!(
                "/control/v1/tenants/{}/configurations/{}",
                harness.fixture.tenant, harness.revision.revision_id
            )),
            Corruption::Route => Some(format!(
                "/control/v1/tenants/{}/resources",
                harness.fixture.tenant
            )),
            Corruption::Credential => None,
        };
        if let Some(path) = read_path {
            let mut request = client
                .get(format!("{}{path}", harness.base))
                .bearer_auth(broken_token)
                .header(
                    "bitrouter-installation-id",
                    harness.fixture.installation.as_str(),
                );
            if matches!(corruption, Corruption::Route) {
                request = request.query(&harness.route);
            }
            let response = request.send().await?;
            ensure!(
                response.status() == StatusCode::SERVICE_UNAVAILABLE,
                "corrupt read status: {}",
                response.status()
            );
            let error = response.text().await?;
            ensure!(
                !error.contains("owned-upstream-secret")
                    && !error.contains("ciphertext")
                    && !error.contains(healthy_token)
            );
            live(&harness)?;
        }
        ensure!(harness.calls.load(Ordering::SeqCst) == 0 && harness.received.try_recv().is_err());

        let mut request = harness.request.clone();
        request["messages"][0]["content"] = json!("healthy-tenant-private");
        let response = client
            .post(format!("{}{}", harness.base, harness.path))
            .headers(healthy_headers)
            .json(&request)
            .send()
            .await?;
        ensure!(
            response.status() == StatusCode::OK,
            "healthy inference status: {}",
            response.status()
        );
        let id = execution_id(&response)?;
        let body = response.text().await?;
        ensure!(body.contains("receipt reply") && !body.contains("broken-tenant-private"));
        tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        let sent = harness
            .received
            .try_recv()
            .context("healthy provider POST")?;
        ensure!(
            sent["model"] == "model-a"
                && sent["messages"][0]["content"] == "healthy-tenant-private"
        );
        ensure!(harness.received.try_recv().is_err());
        let response = client
            .get(format!(
                "{}/control/v1/tenants/{}/executions/{id}",
                harness.base, second.tenant
            ))
            .bearer_auth(healthy_token)
            .header("bitrouter-installation-id", second.installation.as_str())
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let receipt: ExecutionReceipt = response.json().await?;
        ensure!(
            receipt.tenant_id == second.tenant
                && receipt.execution_id == id
                && receipt.execution_revision == revision.revision_id
                && receipt.attempts.len() == 1
        );
        ensure!(
            receipt.attempts[0].outcome == bitrouter_ai::tenancy::execution::Outcome::Completed
        );
        // A valid tenant-B token cannot borrow tenant A's corrupt configuration,
        // and tenant A cannot read the successful tenant-B execution.
        let foreign = headers(&second, healthy_token, &harness.revision.revision_id)?;
        let response = client
            .post(format!("{}{}", harness.base, harness.path))
            .headers(foreign)
            .json(&request)
            .send()
            .await?;
        ensure!(response.status() == StatusCode::NOT_FOUND);
        let _body = response.bytes().await?;
        for (tenant, expected) in [
            (&harness.fixture.tenant, StatusCode::NOT_FOUND),
            (&second.tenant, StatusCode::FORBIDDEN),
        ] {
            let response = client
                .get(format!(
                    "{}/control/v1/tenants/{tenant}/executions/{id}",
                    harness.base
                ))
                .bearer_auth(broken_token)
                .header("bitrouter-installation-id", second.installation.as_str())
                .send()
                .await?;
            ensure!(response.status() == expected);
            let _body = response.bytes().await?;
        }
        if let Some(id) = failed_id {
            let candidate = store::lookup_candidate(
                &harness.fixture.db,
                &harness.fixture.installation,
                &harness.fixture.tenant,
                &id,
            )
            .await?
            .context("failed inference terminal")?;
            ensure!(candidate.receipt.attempts.is_empty());
        }
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        live(&harness)?;
        Ok(())
    }
    .await;
    let stopped = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await;
    checked.with_context(|| format!("same-Core corruption isolation: {corruption:?}"))?;
    stopped.context("isolation shutdown timeout")??;
    Ok(())
}

#[tokio::test]
async fn cold_byok_ciphertext_failure_keeps_other_tenant_inference_live() -> Result<()> {
    check(Corruption::Credential).await
}

#[tokio::test]
async fn cold_route_ciphertext_read_failure_keeps_other_tenant_inference_live() -> Result<()> {
    check(Corruption::Route).await
}

#[tokio::test]
async fn cold_configuration_ciphertext_failure_keeps_other_tenant_inference_live() -> Result<()> {
    check(Corruption::Configuration).await
}

#[tokio::test]
async fn cold_dangling_active_pointer_read_failure_keeps_other_tenant_inference_live() -> Result<()>
{
    check(Corruption::ActivePointer).await
}

async fn check_warm(credential_metadata: bool) -> Result<()> {
    let mut harness = harness().await?;
    let checked: Result<()> = async {
        let second = second_tenant(&harness.fixture).await?;
        let mut route = harness.route.clone();
        route.owner_id = second.tenant.as_str().try_into()?;
        let revision = draft(&second, &route, "warm-isolation-healthy-config", 25).await?;
        let credential = second.issue("warm-isolation-healthy-client").await?;
        let token = harness
            .credential
            .secret
            .as_deref()
            .context("warm token")?
            .to_owned();
        harness.headers = headers(&harness.fixture, &token, &harness.revision.revision_id)?;
        let client = Harness::client(false)?;
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::OK);
        let first = execution_id(&response)?;
        let _body = response.bytes().await?;
        tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
        ensure!(harness.calls.load(Ordering::SeqCst) == 1);
        let _sent = harness.received.try_recv().context("warm provider POST")?;
        // This submission reuses the just-prepared authority view. The valid
        // request also populated the real Service's retained credential cache.
        let held = harness
            .fixture
            .service
            .submission(&harness.headers)
            .await
            .map_err(failure)?;
        let epoch = harness.fixture.service.authority.integrity_epoch();
        let authority_revision = harness.fixture.service.current().map_err(failure)?.revision;
        let mut decoder_root = String::from("credential ciphertext authentication");
        let detected = if credential_metadata {
            let changed = harness
                .fixture
                .db
                .execute(Statement::from_sql_and_values(
                    harness.fixture.db.get_database_backend(),
                    "UPDATE tenancy_credentials SET created_at='not-a-timestamp' WHERE id=?",
                    [harness.credential.credential_id.as_str().into()],
                ))
                .await?;
            ensure!(changed.rows_affected() == 1);
            // Use the same hash-indexed decoder and allocation allowance as the
            // actor, retaining its root cause for a failed detection assertion.
            // This read cannot invalidate Service views on the actor's behalf.
            let decoded = crate::tenancy::store::directory::credential_by_hash_in(
                &harness.fixture.db,
                &crate::auth::keys::hash_key(&token),
                harness.fixture.service.authority.load_budget() / 48,
            )
            .await;
            decoder_root = match decoded {
                Err(error) => format!("invalid_record={}, root={error:#}",
                    error.is::<crate::tenancy::store::directory::InvalidRecord>()),
                Ok(Some(record)) => format!("decoder accepted metadata for expected credential={}",
                    record.id == harness.credential.credential_id),
                Ok(None) => "credential hash lookup returned no row".into(),
            };
            // Force an indexed database read instead of reusing the held view.
            harness
                .fixture
                .service
                .authenticate_with_reuse(&token, false)
                .await
                .map(|_| ())
        } else {
            corrupt(&harness, Corruption::Credential).await?;
            harness
                .fixture
                .service
                .resource_status(
                    &harness.fixture.operator,
                    &harness.fixture.tenant,
                    &harness.slot,
                )
                .await
                .map(|_| ())
        };
        let detected_error = detected.as_ref().err().map(|error| &error.0);
        let detected_code = detected.as_ref().err().map(|error| error.0.code);
        ensure!(
            detected_code == Some(bitrouter_ai::tenancy::operation::ErrorCode::AuthorityUnavailable),
            "fresh detection: error={detected_error:?}, code={detected_code:?}, ok={}, decoder={decoder_root}, epoch_before={epoch}, epoch_after={}, live={}",
            detected.is_ok(),
            harness.fixture.service.authority.integrity_epoch(),
            harness.fixture.service.live.load(Ordering::Acquire),
        );
        ensure!(harness.fixture.service.authority.integrity_epoch() > epoch);
        ensure!(harness.fixture.service.current().map_err(failure)?.revision == authority_revision);
        live(&harness)?;

        let mut held_request = harness.request.clone();
        held_request["messages"][0]["content"] = json!("stale-held-private");
        let input = crate::gateway::http::parse_input(
            &bitrouter_ai::types::ApiProtocol::ChatCompletions,
            held_request.clone(),
        )?;
        let ingress = harness
            .fixture
            .service
            .admission
            .ingress(serde_json::to_vec(&held_request)?.len())
            .map_err(|code| anyhow::anyhow!("held ingress: {code:?}"))?;
        let response = tokio::time::timeout(
            TIMEOUT,
            crate::gateway::http::handle_tenant(
                &harness.fixture.service,
                held,
                harness.headers.clone(),
                bitrouter_ai::types::ApiProtocol::ChatCompletions,
                input,
                ingress,
            ),
        )
        .await?;
        ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
        let _body = axum::body::to_bytes(response.into_body(), 64 * 1024).await?;
        ensure!(
            harness.calls.load(Ordering::SeqCst) == 1,
            "held prepared authority dispatched after corruption was detected"
        );

        // Exercise the normal HTTP same-token authentication/reuse path too:
        // a new lease cannot make the old cached secret usable again.
        let response = harness.send(&client).send().await?;
        ensure!(response.status() == StatusCode::SERVICE_UNAVAILABLE);
        let error = response.text().await?;
        ensure!(!error.contains("owned-upstream-secret") && !error.contains("ciphertext"));
        tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
        ensure!(harness.calls.load(Ordering::SeqCst) == 1 && harness.received.try_recv().is_err());
        live(&harness)?;

        let healthy_token = credential.secret.as_deref().context("healthy token")?;
        let healthy_headers = headers(&second, healthy_token, &revision.revision_id)?;
        let mut request = harness.request.clone();
        request["messages"][0]["content"] = json!("healthy-after-corruption");
        let response = client
            .post(format!("{}{}", harness.base, harness.path))
            .headers(healthy_headers)
            .json(&request)
            .send()
            .await?;
        ensure!(response.status() == StatusCode::OK);
        let id = execution_id(&response)?;
        ensure!(id != first);
        let body = response.text().await?;
        ensure!(body.contains("receipt reply") && !body.contains("stale-held-private"));
        tokio::time::timeout(TIMEOUT, harness.fixture.service.admission.wait_drained()).await?;
        ensure!(harness.calls.load(Ordering::SeqCst) == 2);
        let sent = harness
            .received
            .try_recv()
            .context("healthy provider POST")?;
        ensure!(
            sent["model"] == "model-a"
                && sent["messages"][0]["content"] == "healthy-after-corruption"
        );
        ensure!(harness.received.try_recv().is_err());
        let receipt = store::lookup_candidate(
            &harness.fixture.db,
            &second.installation,
            &second.tenant,
            &id,
        )
        .await?
        .context("healthy terminal after invalidation")?;
        ensure!(
            receipt.receipt.tenant_id == second.tenant
                && receipt.receipt.execution_revision == revision.revision_id
                && receipt.receipt.attempts.len() == 1
                && receipt.receipt.attempts[0].outcome
                    == bitrouter_ai::tenancy::execution::Outcome::Completed
        );
        live(&harness)?;
        Ok(())
    }
    .await;
    let stopped = tokio::time::timeout(TIMEOUT, harness.fixture.service.shutdown()).await;
    checked.with_context(|| {
        format!("warm retained corruption isolation (metadata: {credential_metadata})")
    })?;
    stopped.context("warm isolation shutdown timeout")??;
    Ok(())
}

#[tokio::test]
async fn detected_warm_byok_corruption_blocks_held_and_reused_authority_without_fencing_neighbor()
-> Result<()> {
    check_warm(false).await
}

#[tokio::test]
async fn detected_warm_credential_metadata_corruption_blocks_held_and_reused_authority_without_fencing_neighbor()
-> Result<()> {
    check_warm(true).await
}
